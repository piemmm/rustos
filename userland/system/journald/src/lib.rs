//! RustOS journal service — dispatch core.
//!
//! The journal is the one component that writes authoritative system-log
//! segment files in steady state (SYSLOG §12.1). Ordinary processes never
//! append to a segment directly: they frame a
//! [`LogIngressRequest`](rustos_abi::log_ingress::LogIngressRequest) and post
//! it to the well-known
//! [`LOG_INGRESS_ENDPOINT`](rustos_abi::log_ingress::LOG_INGRESS_ENDPOINT),
//! which the journal service binds.
//!
//! # What this crate is
//!
//! This crate is the **dispatch core**: the architecture-neutral policy layer
//! that turns one framed request, plus the caller's kernel-attested
//! [`Origin`](rustos_abi::Origin), into an admitted, committed record. It owns
//! exactly two things and no I/O:
//!
//! * [`serve`] — decode and validate the request, resolve the advisory
//!   stream/level against the closed vocabularies, admit under the attested
//!   origin (never a caller claim), commit to the injected
//!   [`Journal`](rustos_log::Journal), and author a trusted `security` record
//!   for any spoof attempt. Every path fails closed.
//! * [`store`] — the pure `/System/Logs/<stream>/<id>.seg` path derivation the
//!   production filesystem sink uses.
//!
//! The persistence engine ([`Journal`](rustos_log::Journal)), the record
//! model, the stream/level vocabulary, and the admission authority all live in
//! [`rustos_log`]; this crate is the thin, exhaustively-testable broker over
//! them. The service *binary* — which binds the endpoint, reads each caller's
//! peer origin, loads the installation's attestation key, and drives this core
//! over the real filesystem — is a staged follow-on (SYSLOG §15).
//!
//! # Layering
//!
//! The crate is `no_std` (with `alloc`) and depends only on the audited
//! `lib/*` crates `rustos-abi` and `rustos-log`, so it never links a kernel or
//! driver crate.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod service;
pub mod store;

pub use service::{serve, Clock, Ingest};
