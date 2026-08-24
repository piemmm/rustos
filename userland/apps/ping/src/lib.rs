//! TAIRiX `ping` — send ICMP/`ICMPv6` echo requests (`plans/NETWORK.md`
//! N8b-2b, a `plans/APPS.md` command app).
//!
//! `ping` measures reachability and round-trip time to a host in the
//! familiar iputils shape: one line per reply with its sequence number and
//! time, then a closing statistics block. `-c` bounds the request count,
//! `-i` sets the interval, `-s` the payload size, `-W` the per-reply
//! timeout, `-w` an overall deadline, `-4`/`-6` force the family, `-q` is
//! quiet, and `-n` is numeric (always in force — there is no name resolver
//! in this plan, so the target must be a literal address). `-?`/`--help`
//! render the tool's own short help through the shared `lib/help` engine.
//!
//! # How it reaches the network
//!
//! The tool opens an ICMP/`ICMPv6` **echo socket** through the versioned
//! `netsock-v1` socket ABI served by `userland/net/netstack` — gated on
//! `CAP_NET` + `CAP_NET_RAW`, audited, fail-closed. It never crafts a raw IP
//! packet, never touches a device, and holds no ambient authority: the stack
//! owns the ICMP identifier (so a socket only ever receives replies to its own
//! requests) and every capability and input check stays stack- and kernel-side.
//!
//! # What this crate is
//!
//! A **ping engine**, not a data source. For one parsed [`Command`] it
//! sends each echo request, awaits the matching reply within the timeout,
//! prints each result, and prints the statistics. The operations that touch
//! the outside world are the injected seams, so the whole engine is
//! host-testable with no kernel:
//!
//! * [`net::PingIo`] — the clock, the echo socket, and the wait/park.
//! * [`Output`] — the per-reply lines and statistics on standard output,
//!   diagnostics on standard error.
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree, read by the
//!   short-help switches.
//!
//! The bundle's `Help/` documents are **not** embedded in this crate: they
//! are authored once in the bundle's on-disk `Help/` tree, planted onto
//! `/System` by the image builder from that source (`tools/syshelp`), and
//! read back at runtime through the injected [`tairix_help::HelpSource`]
//! seam. Help is never hardcoded into the program (`plans/APPS.md`).
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the dependencies are the audited `lib/abi`
//! vocabulary and the shared `lib/help` engine, so this userland tool never
//! links a kernel or driver crate. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod io;
pub mod net;

pub use client::{run, RunSummary, USAGE};
pub use command::{parse, Command, Config, ParseError, Target};
pub use error::PingError;
pub use io::Output;
pub use net::{EchoReply, PingIo};
