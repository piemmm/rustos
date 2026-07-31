//! TAIRiX Switchboard monitor service: the sampler/derive/publish/schedule
//! core behind the always-right-most Switchboard taskbar icon
//! (`plans/NEW-TASKBAR.md` T10).
//!
//! # What this service is
//!
//! The Switchboard is its own dedicated, capability-sized process
//! (`userland/gui/switchboard`), separate from the desktop session, exactly
//! so the session's own manifest never has to grow the system-wide sysinfo
//! and process-control authority the tray overview needs. It samples the
//! live system through the System Information API (never a private
//! back-channel, never `/proc`/`/sys`) and publishes a compact
//! [`tairix_abi::switchboard_ipc::TraySummary`] to the desktop session over
//! the seat-scoped `SWITCHBOARD_ENDPOINT` the session binds. Publication is
//! event-driven on change, with a bounded keepalive fallback — never
//! polled on a fixed cadence for its own sake.
//!
//! # Module map
//!
//! * [`sample`] — [`sample::Sampler`], which walks the live process list
//!   and the aggregate CPU/memory-pressure queries and turns them into a
//!   [`sample::Sample`], degrading every field to its honest empty form on
//!   a query failure or a refused capability rather than ever fabricating
//!   one.
//! * [`mod@derive`] — [`derive::derive_summary`], the pure function that turns
//!   a [`sample::Sample`] plus the running [`derive::Hysteresis`] state
//!   into the wire [`tairix_abi::switchboard_ipc::TraySummary`].
//! * [`publish`] — [`publish::Publisher`], the change-only/keepalive
//!   publish gate and consecutive-failure counter.
//! * [`schedule`] — the pure, drift-free next-sample-deadline computation,
//!   so the service's run loop issues exactly one wait per iteration with a
//!   timeout equal to "time until the next thing that must happen" —
//!   tickless, never a busy poll.
//!
//! # Honest-data rules
//!
//! Every field this crate produces is either a real measurement or an
//! explicit absence (`None`, or a documented honest zero such as `jobs`,
//! for which no background-job registry exists in the OS today); nothing
//! here ever synthesises a plausible-looking value to fill a gap. A denied
//! or failed query degrades the one field it backs, is noted once (never
//! spammed) at the layer that has a stream to write to, and the sampler
//! keeps producing every other field truthfully.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); this crate depends only on the audited
//! `tairix-abi` wire types and the shared `tairix-procinfo` System
//! Information API client helpers, and links no kernel or driver crate.
//! No `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths. The
//! freestanding `Run` binary (`src/run.rs`) is documented separately; this
//! library is the host-testable core it drives.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

pub mod derive;
pub mod publish;
pub mod sample;
pub mod schedule;

pub use derive::{derive_summary, Hysteresis};
pub use publish::Publisher;
pub use sample::{
    probe_scopes, DegradedField, MemoryPressureSample, Sample, Sampler, ScopeVerdicts, TopTask,
};
pub use schedule::{advance_deadline, wait_timeout_ns, MEMORY_SAMPLE_DIVIDER, SAMPLE_PERIOD_NS};
