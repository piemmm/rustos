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
//! * [`command`] — [`command::authenticate_command`], which drops any
//!   command not attested to the desktop session's identity before it is
//!   ever decoded, let alone applied.
//! * [`activities`] — [`activities::Activities`], the service-held,
//!   session-lifetime state that groups live processes into named
//!   activities the Activities section renders and acts on.
//! * [`model`] — [`model::build_model`], the pure function that turns a
//!   [`sample::Sample`], the rolling [`model::LiveMeters`] state, the
//!   session's [`tairix_abi::switchboard_ipc::SeatReport`], and the
//!   service's [`activities::Activities`] into the live overview panel's
//!   [`tairix_controls::SwitchboardModel`], and [`model::apply_action`],
//!   which maps the panel's reported [`tairix_controls::SwitchboardAction`]
//!   back onto the outbound [`model::Effect`]s it implies.
//! * [`panel`] — [`panel::Panel`], the overview window's lifecycle (open,
//!   raise, refresh, close) and effect application.
//! * [`service`] — [`service::ServiceHost`], the single seam through which
//!   everything outside this process is reached, and [`service::Service`],
//!   the run loop's body: sample, derive, record, prune and refresh the
//!   activity-grouping state, refresh the panel, and publish — every
//!   cycle, whether or not a window is open.
//! * [`wait`] — the wait-set token vocabulary the run loop's single
//!   multiplexed park covers: its own termination signal, the session's
//!   command mailbox, and — only while a window is open — that window's
//!   event mailbox.
//!
//! # Honest-data rules
//!
//! Every field this crate produces is either a real measurement or an
//! explicit absence (`None`, a [`tairix_controls::MeterValue::Unmeasured`]
//! meter, or a documented honest zero such as `jobs`); nothing here ever
//! synthesises a plausible-looking value to fill a gap. A denied or failed
//! query degrades the one field it backs, is noted once (never spammed) at
//! the layer that has a stream to write to, and the sampler keeps producing
//! every other field truthfully.
//!
//! Three parts of the overview panel are therefore always empty, because
//! the interfaces that would fill them do not exist:
//!
//! * **Background jobs.** There is no background-job registry anywhere in
//!   the OS to enumerate, so the tray summary's `jobs` count is an honest
//!   zero and the panel's Jobs section has no rows.
//! * **Services.** The System Information API
//!   ([`tairix_abi::sysinfo`]) has no service-enumeration query — its
//!   queries cover processes, CPU time, and memory pressure — so the
//!   Overview section's service list stays empty rather than listing
//!   guesses drawn from process names.
//! * **System actions.** There is no power or session-lock interface this
//!   service may drive, so it offers no shut-down, restart, or lock
//!   button. Offering one it could not perform would be an action that
//!   fails at the point of use rather than being honestly absent.
//!
//! A disk resource row is absent for the same reason: the System
//! Information API exposes no disk-throughput query. A network resource
//! row is absent for a different reason — the System Information API does
//! carry a live throughput query
//! ([`tairix_abi::sysinfo::SysinfoQueryId::NET_INTERFACE_RATES`]), but this
//! service does not consume it: that query's capability gate and sampling
//! budget belong to the tray's own pressure latches, and no CPU/memory-style
//! pressure latch exists yet for network, so there is nothing this crate
//! could honestly render a rail or a card from.
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

pub mod activities;
pub mod command;
pub mod derive;
pub mod model;
pub mod panel;
pub mod publish;
pub mod sample;
pub mod schedule;
pub mod service;
pub mod wait;

#[cfg(test)]
mod test_host;

pub use activities::{
    Activities, ActivityView, Member, ACTIVITY_NAME_MAX, MAX_ACTIVITIES, MAX_ACTIVITY_MEMBERS,
};
pub use command::{authenticate_command, is_from_session};
pub use derive::{derive_summary, memory_pressured, Hysteresis};
pub use model::{
    apply_action, build_model, derive_self_uid, map_section, signal_pid, Effect, GroupingEdit,
    LiveMeters, PanelModel,
};
pub use panel::{refusal_notice, CommandOutcome, Panel, PanelOutcome, PANEL_TITLE};
pub use publish::Publisher;
pub use sample::{
    probe_scopes, DegradedField, MemoryPressureSample, ProcessSummary, Sample, Sampler,
    ScopeVerdicts, TopTask,
};
pub use schedule::{advance_deadline, wait_timeout_ns, MEMORY_SAMPLE_DIVIDER, SAMPLE_PERIOD_NS};
pub use service::{CycleOutcome, Service, ServiceHost, MAX_CONSECUTIVE_PUBLISH_FAILURES};
pub use wait::{required_members, WaitToken};
