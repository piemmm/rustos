//! RustOS `sysmon` — a live, fullscreen kernel-memory and load monitor
//! (`plans/STRESSTEST.md` ST4).
//!
//! `sysmon` observes every aspect of the kernel's memory through the System
//! Information API — physical memory, the kernel heap, the memory-pressure
//! bands with their history, the reclaimable-cache ledger, the `ramzip`
//! compressed tier, the pinned aggregate, and per-CPU load — usable while
//! the system is under deliberate stress (its primary function) and at
//! idle. It pins its own memory at startup (`mem_pin`) so it never stalls
//! on its own page fault-in under the very pressure it observes; a refused
//! pin is reported on the title line and the session continues unpinned.
//!
//! # What this crate is
//!
//! A thin, fully host-testable front end built from three seams:
//!
//! * [`Transport`] — the `sysinfo` request channel (from `lib/procinfo`),
//!   shared with `top`/`ps`/`sysinfo`; the four kernel-statistics fetches
//!   are the shared `kstats` walks, never a private copy.
//! * [`Tty`] — the curses byte channel; an in-memory channel makes the
//!   whole monitor testable without a kernel.
//! * [`Model`] — the I/O-free view state (snapshot, panel focus, scroll,
//!   interval, pin state, help) that [`render`] draws and [`run`] drives.
//!
//! # Module map
//!
//! * [`error`] — [`SysmonError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] shape and its [`parse`]r: the reserved
//!   `-h`/`-?` short-help switches (plans/APPS.md §4) and the GNU
//!   `-d`/`--delay` refresh interval against running the monitor.
//! * [`model`] — the [`Model`], its [`Focus`]/[`Gauge`]/[`PinState`], and
//!   the [`Action`] an event produces.
//! * [`app`] — [`render`] and the [`run`] input loop.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`). It links only `lib/*` crates — the audited
//! `lib/abi`, the shared `lib/procinfo`, and the OS-provided
//! `lib/curses`/`lib/termcap`/`lib/vt` — never a kernel or driver crate.
//! `lib/curses` is the curated `/System/Libraries/` Terminal/TUI class,
//! dynamically linked at runtime; in the workspace it is an ordinary cargo
//! path dependency. No `unsafe`, and no `unwrap`/`expect`/`panic!` in
//! production paths; nothing writes to fd 3 (`stdinfo`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod app;
pub mod command;
pub mod error;
pub mod model;

#[cfg(test)]
mod tests;

pub use app::{detail_capacity, render, run};
pub use command::{
    parse, Command, DEFAULT_DELAY_TENTHS, DELAY_STEP_TENTHS, MAX_DELAY_TENTHS, MIN_DELAY_TENTHS,
    USAGE,
};
pub use error::SysmonError;
pub use model::{Action, CpuBusy, Focus, Gauge, Model, PinState, Snapshot, BAND_HISTORY_MAX};
pub use rustos_curses::{Screen, Tty};
pub use rustos_procinfo::Transport;
