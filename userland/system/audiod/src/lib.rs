//! `audiod` — the one mixer, router and audio authority (`plans/SOUND.md`).
//!
//! One system service, not one per user: the device is machine state, so the
//! arbiter is a machine service with per-seat routing and per-principal
//! accounting. It is the **sole holder of `CAP_AUDIO_DEVICE`** and the only
//! process that speaks `audiochan-v1`; every program reaches sound through
//! `audio-v1` and there is no second path.
//!
//! # The shape
//!
//! | Piece | What it is |
//! |---|---|
//! | [`AudioChannelClient`] | The `audiochan-v1` client half: one `ipc_call` per control operation, over an injected transport. |
//! | [`RegionHost`] | The shared-region seam. Regions travel in **both** directions (see below). |
//! | [`AudioService`] | The engine: devices, endpoints, streams, the period pump, and the `audio-v1` request handler. |
//!
//! Everything that decides *what samples come out* is `lib/audio`'s and is
//! composed here rather than re-derived: [`route`](tairix_audio::route) for
//! policy, [`ChannelMatrix`](tairix_audio::ChannelMatrix) at open,
//! [`Mixer`](tairix_audio::Mixer) for the period,
//! [`ClockModel`](tairix_audio::ClockModel) per endpoint,
//! [`volume::resolve`](tairix_audio::volume::resolve) for the one multiply,
//! and `convert`/`resample` for a rate or format mismatch. This crate owns no
//! sample arithmetic of its own.
//!
//! # Regions run in both directions
//!
//! * A **client ring** is created by the *client*, which `shm_grant`s it to
//!   this service; [`AudioRequest::Attach`](tairix_abi::audio::AudioRequest::Attach)
//!   carries the handle and the service maps it, validating the mapped length
//!   against the geometry it granted before a frame moves.
//! * A **device ring** is created by *this service* and granted to the
//!   driver's endpoint; [`AttachParams::region_grant`](tairix_abi::driver::audio_channel::AttachParams::region_grant)
//!   carries the handle and the driver maps it.
//!
//! Getting that backwards would hand a client's process a window into a
//! driver's ring, so the seam spells the two separately ([`RegionHost::create`]
//! versus [`RegionHost::adopt`]) rather than taking a handle of unstated
//! provenance.
//!
//! # Real-time discipline
//!
//! Every buffer is allocated at stream-open or device-configure and reused;
//! the per-period path allocates nothing. It reads each client ring into that
//! stream's own scratch, so exactly **one** region is borrowed at a time —
//! which [`RegionHost::bytes`] enforces by taking `&mut self`. The service
//! parks on {device notify, client doorbells, control endpoint} and never
//! spins: the device's own period interrupt is the only timer in the stack.
//!
//! # Fail closed
//!
//! Every `audio-v1` reply is a fully-encoded frame carrying a typed
//! [`Errno`](tairix_abi::Errno). A capture open without `CAP_AUDIO_CAPTURE`,
//! a stream id that is not the caller's, a region whose length does not match
//! the agreed geometry, a device that faulted — each is a refusal, never a
//! panic and never a partially-applied action. Every security decision lands
//! on the audit log with a stable event id, and a capture *refusal* is
//! recorded as firmly as a grant: "who tried" is the question an incident
//! asks.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

mod channel;
mod device;
pub mod events;
mod region;
mod service;

pub use channel::{AudioChannelClient, AudioChannelTransport};
pub use region::{RegionHost, RegionId};
pub use service::{AudioService, Caller, Notifier};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

/// The reserved, fail-closed process exit codes the audio service's `Run`
/// binary ends with when it cannot serve.
///
/// The numbers match `lib/netchan`'s and `lib/audiochan`'s, so one supervisor
/// table reads every class of service failure.
pub mod exit {
    /// The reserved `audio-v1` rendezvous could not be claimed: the bind was
    /// refused (another process holds it, or this one lacks the privileged
    /// bind), or the notify port could not be bound.
    pub const NO_SERVICE: i32 = 83;
    /// The wait set the service parks on could not be built, so the loop
    /// would have had to poll.
    pub const NO_RESOURCES: i32 = 81;
}
