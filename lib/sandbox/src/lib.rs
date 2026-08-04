//! TAIRiX parser-sandbox seam (`lib/sandbox`).
//!
//! Every parser of untrusted input runs in a minimum-capability sandbox
//! process; the kernel primitive that creates such a process is the
//! `SPAWN_FLAG_SANDBOX` spawn mode (`docs/src/security/sandbox.md`). This
//! crate is the one user-space seam over that primitive — the typed
//! request/reply path every program that sandboxes a parse imports, so the
//! containment discipline is written once:
//!
//! * [`proto`] — the length-framed byte protocol over any [`proto::Channel`]
//!   (pipes in production, in-memory fakes in host tests), bounded by
//!   [`proto::MAX_FRAME`].
//! * [`worker`] — the serve loop the sandboxed process runs over a total
//!   [`worker::Service`].
//! * [`host`] — the calling program's side: [`host::ParserSandbox`] sends a
//!   request and contains every worker failure — typed error to the
//!   caller, the dead worker reaped and **replaced**, the event logged
//!   with a stable id ([`host::EVENT_WORKER_CRASHED`],
//!   [`host::EVENT_WORKER_UNAVAILABLE`]). A parser crash never takes down
//!   the calling program.
//! * [`helpdoc`] — a foreign bundle's help document is parsed and rendered
//!   inside the worker (`tairix-help`), and the caller-side
//!   [`helpdoc::render_help`] re-parses the reply through the `tairix-vt`
//!   streaming parser, admitting only the closed render-op set a help
//!   render can legitimately contain.
//! * [`loopback`] — the public in-process fake, so a consumer's host tests
//!   run the full parent path without processes (the `Fs`/`Tty` seam
//!   pattern).
//! * [`decode`] — the first consumers behind the seam: executable-container
//!   summaries (`tairix-binfmt`) and instruction windows (`tairix-disasm`),
//!   with fail-closed reply validation (the worker is hostile once it has
//!   parsed a byte).
//! * [`imagerender`] — an application bundle's icon (SVG or PNG bytes) is
//!   sniffed, decoded, and rasterised inside the worker
//!   (`tairix-svg`/`tairix-image`/`tairix-icon`/`tairix-raster`), and the
//!   caller-side [`imagerender::rasterise_icon`] validates the reply's echoed
//!   side and pixel length before trusting the returned RGBA8 buffer. The
//!   same worker also places a desktop wallpaper
//!   (`tairix-image`/`tairix-wallpaper`/`tairix-raster`) across a
//!   prepare/band/release sequence, bounded by
//!   [`crate::proto::MAX_FRAME`]; the caller-side
//!   [`imagerender::render_wallpaper`] drives the sequence and validates
//!   every band's echoed geometry and exact pixel length before trusting
//!   the assembled RGBA8 buffer.
//! * `rt` (feature `program`, freestanding only; not compiled on hosted
//!   targets) — the production transport: the parent spawns its own binary
//!   in the worker role over a pipe pair wired through
//!   `SpawnAttach::sandbox`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod decode;
pub mod helpdoc;
pub mod host;
pub mod imagerender;
pub mod loopback;
pub mod proto;
#[cfg(all(freestanding, feature = "program"))]
pub mod rt;
pub mod wire;
pub mod worker;

pub use host::{Launcher, ParserSandbox, SandboxError};
pub use proto::{Channel, ProtoError, MAX_FRAME};
pub use worker::{serve, ServeEnd, Service};
