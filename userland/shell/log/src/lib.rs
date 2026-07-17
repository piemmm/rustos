//! TAIRiX `log` — the terminal reader, renderer, and verifier of the system
//! log (SYSLOG §14).
//!
//! The system log is a set of immutable, append-only, hash-chained segment
//! files under `/System/Logs/<stream>/`, one per stream. `log` is the
//! command-line tool that reads those files and turns them into readable
//! output — or checks their integrity. It never reads `/proc`, `/sys`, or a
//! device path (there are none); the authoritative data is the segment files
//! and the TAIRiX APIs (SYSLOG §14).
//!
//! # What this crate is
//!
//! A **read/render/verify state machine**, not a data source. For one parsed
//! [`Command`] it reads a stream's segments one at a time through the injected
//! [`SegmentSource`] seam, decodes each committed record with the shared
//! `lib/log` model, and either renders it — through the shared boot/rich
//! renderers — to the injected [`Output`] seam, or verifies its integrity with
//! the shared segment verifier. Nothing about the on-disk format is
//! re-implemented here; the tool consumes `lib/log` rather than carrying a
//! second copy of it.
//!
//! The two operations that touch the outside world — reading segments and
//! writing the terminal — are the injected [`SegmentSource`] and [`Output`]
//! seams. The binary that ships as `log` wires the real syscall-backed
//! `/System/Logs` reader and console; tests wire in-memory fixtures. This is
//! the seam discipline of the other userland crates (`cat`'s `FileSource`,
//! `ls`'s `Listing`, `sysinfo`'s `Transport`), and it keeps every parsing,
//! rendering, and verification decision testable without a kernel.
//!
//! # Security properties (inherited from the renderers)
//!
//! * **Provenance is preserved, never obeyed.** System-attested facts are kept
//!   separate from caller content; a caller's *requested* privileged
//!   source/stream is shown inertly as a claim, never promoted.
//! * **Caller text can never forge output.** Control characters and quotes in
//!   caller-controlled strings are escaped by the renderers, so a hostile
//!   record cannot move the cursor, forge a prefix, or break the JSON.
//! * **Verification fails closed.** A corrupt or tampered segment is reported
//!   and produces a non-zero result; a sealed (`audit`/`security`) stream
//!   cannot be verified without the attestation key rather than passing
//!   unchecked (SYSLOG §13).
//!
//! # Module map
//!
//! * [`error`] — [`LogError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`]/[`Format`] shapes and their [`parse`]r.
//! * [`io`] — the [`SegmentSource`] and [`Output`] seams.
//! * [`client`] — the [`run`] entry point and the render/verify engine.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the audited `lib/abi`
//! ABI crate and the shared `lib/log` model, so this userland tool never links
//! a kernel or driver crate. No `unsafe`, and no `unwrap`/`expect`/`panic!` in
//! production paths.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod io;

pub use client::{run, USAGE};
pub use command::{parse, Command, Format};
pub use error::LogError;
pub use io::{Output, SegmentSource};
