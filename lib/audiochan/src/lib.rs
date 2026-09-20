//! The **driver side** of the `audiochan-v1` audio device-channel contract
//! (`plans/SOUND.md` SND4).
//!
//! The mixer service (`userland/system/audiod`) owns the shared PCM regions
//! and is the channel's *client*; an audio driver process owns the device
//! (MMIO/DMA/IRQ) and is its *server*. This crate is that server, written
//! once over the [`Audio`](tairix_abi::driver::audio::Audio) trait so every
//! audio driver process — whatever silicon it drives — shares one control
//! plane instead of re-deriving it. It is separate from `lib/audio` for the
//! reason `lib/netchan` is separate from `lib/net`: a driver process must not
//! link the mixer.
//!
//! Two layers:
//!
//! * [`AudioChannelServer`] — the pure, host-testable per-request handler:
//!   per-endpoint configuration and attach state, geometry validation, and
//!   the service logic. No I/O, so the whole control plane is exercised on
//!   the host against a mock device.
//! * `serve` — the freestanding process loop the driver binary hands its
//!   opened device to: claim a reserved device-channel endpoint, publish the
//!   [`AUDIOCHAN_NODE_COMPATIBLE`](tairix_abi::driver::audio_channel::AUDIOCHAN_NODE_COMPATIBLE)
//!   hardware-tree node carrying it, and park on a wait set over {call
//!   endpoint, device interrupt} for the life of the driver. Compiled only
//!   for the bare-metal targets a driver binary is built for; the host build
//!   carries just the pure handler.
//!
//! # Nothing spins
//!
//! Between doorbells the driver parks on its device interrupt. A period
//! boundary wakes it, it moves that period between the shared ring and the
//! device, and it sends one notify carrying the clock pair the mixer's linear
//! fit is built from. There is no audio tick and no poll loop: the device's
//! own period interrupt is the only timer in the stack.
//!
//! # Fail closed
//!
//! Every reply is a fully-encoded `audiochan-v1` frame carrying a typed
//! [`Errno`](tairix_abi::Errno) — a service call before attach, a region too
//! small for the agreed geometry, an endpoint index the device does not
//! present, or a device fault is never a panic and never a partially-applied
//! action. Every set-up refusal in `serve` exits with a reserved code from
//! [`exit`] rather than degrading.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

mod server;
pub use server::{AudioChannelServer, Serviced};

#[cfg(target_os = "none")]
mod serve;
#[cfg(target_os = "none")]
pub use serve::{fail, serve};

/// The reserved, fail-closed process exit codes an audio driver binary ends
/// with when it cannot serve its device.
///
/// One definition for every audio driver process: the codes are the diagnosis
/// a supervisor reads off a driver that gave up, so two drivers reporting the
/// same failure must report the same number. `serve` returns
/// [`NO_SERVICE`](exit::NO_SERVICE) itself; the others are returned by the
/// binary's own bring-up, before it hands the device over. The numbers match
/// `lib/netchan`'s, so one supervisor table reads both classes.
pub mod exit {
    /// The rt-backed driver host could not be built from the
    /// kernel-delivered grants (the `resource_grants` query was refused, or
    /// the delivery did not fit).
    pub const NO_HOST: i32 = 80;
    /// The delivered grants do not name the resources this driver needs — an
    /// unbound, mis-provisioned, or malformed node.
    pub const NO_RESOURCES: i32 = 81;
    /// Device bring-up failed: a window could not be mapped, the device is
    /// not the one the node claimed, it rejected its init sequence, or the
    /// granted interrupt line could not be bound (the serve loop parks on
    /// it, so a driver that cannot bind it would degrade into the busy
    /// re-poll the charter forbids).
    pub const BRINGUP_FAILED: i32 = 82;
    /// The device channel could not be stood up: no free reserved endpoint
    /// id, the bind was refused, the `audiochan` node could not be published,
    /// or the wait set could not be built.
    pub const NO_SERVICE: i32 = 83;
}
