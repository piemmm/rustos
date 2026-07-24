//! TAIRiX `ss` — list open sockets (`plans/NETWORK.md` N8b-2, a
//! `plans/APPS.md` command app).
//!
//! `ss` lists the system's open sockets in the familiar iproute2 shape:
//! one row per socket with its protocol, state, receive/send queue depths,
//! local and peer address:port, and (with `-p`) its owning process. The
//! default view shows connected, non-listening sockets; `-l` shows only
//! listening sockets and `-a` shows both, `-t`/`-u` filter by protocol,
//! `-4`/`-6` by address family, `-n` is numeric (always in force — TAIRiX
//! has no service-name database), and `-H` drops the header. `-?`/`--help`
//! render the tool's own short help from its bundled `Help/` tree through
//! the shared `lib/help` engine.
//!
//! # Where the rows come from
//!
//! Live system state is read exclusively through the System Information
//! API (`AGENTS.md` §16.6): the typed, versioned `sysinfo-v1` `NET_SOCKETS`
//! query served by `/System/Services/sysinfod.app/Run`, which forwards to
//! `netstack`'s capability-gated broker read. There is no `/proc/net` and
//! no second query client — the paging walk is the shared
//! `tairix_procinfo::for_each_net_socket`. The query names every
//! principal's sockets and every connection's peer address, so it is
//! gated on `CAP_SYSINFO_GLOBAL` and audited; a session without that
//! capability is told so on standard error and the tool exits, rather than
//! printing an empty table a reader would mistake for "no sockets".
//!
//! # What this crate is
//!
//! A **selection and rendering engine**, not a data source. For one
//! parsed [`Command`] it fetches the socket table, keeps the rows the
//! filters select, and renders the iproute2-shaped listing. The
//! operations that touch the outside world are the injected seams:
//!
//! * [`tairix_procinfo::Transport`] — the `NET_SOCKETS` query.
//! * [`Output`] — the table on standard output, diagnostics on standard
//!   error, and the omission advisory on fd 3.
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree, read by
//!   the short-help switches.
//!
//! # Advisory output
//!
//! When the default view hides listening sockets, `ss` notes the omission
//! on the standard information stream (fd 3) with the
//! `net.listening_omitted` record (`AGENTS.md` §20.1) — advisory only,
//! never affecting the table, the ordering, or the exit status.
//!
//! The bundle's `Help/` documents are **not** embedded in this crate:
//! they are authored once in the bundle's on-disk `Help/` tree, planted
//! onto `/System` by the image builder from that source (`tools/syshelp`),
//! and read back at runtime through the injected
//! [`tairix_help::HelpSource`] seam. Help is never hardcoded into the
//! program (`plans/APPS.md`).
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the dependencies are the audited `lib/abi`
//! vocabulary, the shared `lib/help` engine, and the shared `lib/procinfo`
//! sysinfo client, so this userland tool never links a kernel or driver
//! crate. No `unsafe`, and no `unwrap`/`expect`/`panic!` in production
//! paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod io;

pub use client::{run, USAGE};
pub use command::{parse, Command, Options, ParseError};
pub use error::SsError;
pub use io::Output;
