//! TAIRiX `telnet` — the RFC 854 Network Virtual Terminal client
//! (`plans/TELNET.md`, a `plans/APPS.md` command app).
//!
//! `telnet` opens a TCP connection to a host and relays the terminal to it in
//! the familiar BSD/inetutils shape: the remote host's output appears on
//! standard output, keystrokes go to the host, and the escape character
//! (`^]` by default) drops into the `telnet>` command interpreter. It is both
//! the way to reach a line-oriented service on another machine and the way to
//! poke any TCP service by hand (`telnet host 80`).
//!
//! # How it reaches the network
//!
//! The tool opens a `SocketType::Stream` socket through the versioned
//! `netsock-v1` socket ABI served by `userland/net/netstack` — gated on
//! `CAP_NET`, audited, fail-closed. A host *name* is resolved through the
//! shared userland stub resolver ([`tairix_resolver`]), which reads the
//! configured recursive servers from the System Information API and drives the
//! RFC 1035 engine in `lib/net` over an ordinary UDP socket. There is no second
//! DNS implementation, no raw packet access, and no ambient authority.
//!
//! # What this crate is
//!
//! A **protocol engine**, not a program. Every byte the remote host sends is
//! attacker-controlled, so the codec is total, bounded and fail-closed
//! ([`nvt`]), the negotiation cannot be made to loop ([`option`], the RFC 1143
//! Q Method), and every option payload is validated before it is acted on
//! ([`subneg`], [`linemode`]). The operations that touch the outside world are
//! injected seams, so the whole engine — including the interactive relay — is
//! host-testable with no kernel and no network:
//!
//! * [`net::TelnetIo`] — the connection, the terminal, and the one ordered
//!   event stream the relay reacts to. The production implementation
//!   (`src/run.rs`) parks a single wait-set over both the socket's delivery
//!   port and the port its keyboard-reader thread posts to, so neither
//!   direction is ever polled.
//! * [`Output`] — the host's output and the interpreter's replies on standard
//!   output, diagnostics on standard error.
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree, read by the
//!   short-help switches.
//!
//! The bundle's `Help/` documents are **not** embedded in this crate: they are
//! authored once in the bundle's on-disk `Help/` tree, planted onto `/System`
//! by the image builder, and read back at runtime through the injected
//! [`tairix_help::HelpSource`] seam (`plans/APPS.md`).
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the dependencies are the audited `lib/abi`
//! vocabulary, the shared `lib/help` engine, the `lib/net` DNS types, the
//! `lib/resolver` client, and `lib/vt`'s terminal control vocabulary — so this
//! userland tool never links a kernel or driver crate. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod edit;
pub mod error;
pub mod interp;
pub mod io;
pub mod linemode;
pub mod net;
pub mod nvt;
pub mod option;
pub mod session;
pub mod subneg;

pub use client::{run, FALLBACK_TERM, USAGE};
pub use command::{parse, Command, Config, ParseError, Target};
pub use error::TelnetError;
pub use io::Output;
pub use net::{CloseReason, IoEvent, TelnetIo};
pub use session::{KeyFold, NetFold, Relay, Session};
