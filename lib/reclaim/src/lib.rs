//! The shared reclaimable-memory model (`plans/SMARTRAM.md`).
//!
//! TAIRiX treats spare RAM as a bounded, owner-charged, reclaimable set
//! of caches rather than an unbounded free-for-all. Every one of those
//! caches — the kernel's clean filesystem, block, transform, and
//! semantic-launch caches, and the desktop session's rasterised-asset
//! caches — obeys one model, defined here so it cannot fork:
//!
//! - [`model`] — what a cache *is*: its [`ReclaimClass`], its
//!   [`ReclaimOwner`], how expensive it is to rebuild, how sensitive
//!   its bytes are, what invalidates it, how it must be released, the
//!   fail-closed [`classify`](CacheCandidate::classify) gate that
//!   refuses anything under-declared, the [`CacheBudget`] derived from
//!   its backing, and the checked [`CacheAccounting`] ledger it charges
//!   every entry to.
//! - [`pressure`] — the five-band memory-pressure vocabulary shared
//!   with `plans/SWAPSWAPSWAP.md`, the hysteresis thresholds, the
//!   measuring [`MemoryPressure`] gauge and the receiving
//!   [`ReportedPressure`] one behind a single [`PressureGauge`]
//!   interface, and the [`shrink_target`] ordering that decides which
//!   class gives its memory back first.
//! - [`cache`] — [`ReclaimCache`], the one implementation of all of the
//!   above: a bounded, generation-invalidated, pressure-governed,
//!   self-poisoning LRU cache a consumer parameterises with its own
//!   key, value, and generation.
//! - [`audit`] — the stable audit events a classification refusal or a
//!   detected ledger defect emits.
//! - [`desktop`] — the one desktop-session disposable-UI cache policy:
//!   the classification a rasterised cursor or icon glyph shares, and the
//!   two constructors (wired to a real backing size and pressure gauge, or
//!   an unwired fallback) both the window manager's cursor cache and the
//!   taskbar's icon cache build from.
//!
//! # Why a shared crate and not a kernel module
//!
//! Memory pressure is a property of the machine, not of privilege
//! level. A desktop session holding megabytes of rasterised glyphs and
//! icons must give them back under the same policy, in the same order,
//! at the same bands as the kernel's own caches — otherwise the system
//! has two competing notions of "low on memory" and the reclaim
//! ordering `plans/SMARTRAM.md` section 7 specifies is a fiction. The
//! kernel measures and publishes the band; a process is told it and
//! obeys the same [`shrink_target`]. One model, two vantage points.
//!
//! # What lives elsewhere
//!
//! The `ramzip` handoff and the VM escalation ladder need the kernel's
//! own anonymous-memory tier and stay in `kernel/mem::pressure`. The
//! physical frame allocator implements [`FreeMemorySource`] there too.

#![no_std]

extern crate alloc;

pub mod audit;
pub mod cache;
pub mod desktop;
pub mod ledger;
pub mod model;
pub mod pressure;

pub use audit::{log_cache_poisoned, log_cache_refused, ReclaimAuditEvent};
pub use cache::{CachedBytes, ReclaimCache, Served};
pub use desktop::{
    disposable_ui_cache, disposable_ui_candidate, screenful_ui_cache, working_set_ui_cache,
};
pub use ledger::CacheLedger;
pub use model::{
    AccountingError, AdmissionRefusal, CacheAccounting, CacheBudget, CacheCandidate, CachePolicy,
    InvalidationSource, RebuildCost, ReclaimClass, ReclaimClassStats, ReclaimOwner, ReclaimRule,
    Sensitivity, UI_CACHE_RESERVE_BYTES,
};
pub use pressure::{
    shrink_target, BandObserver, FreeMemorySource, GrowthAllowance, MemoryPressure, PressureBand,
    PressureGauge, PressureThresholds, ReportedPressure, RESERVE_DIVISOR,
};
