//! The display-service protocol engine (`plans/DISPLAY.md` D7b).
//!
//! One crate hosts **both halves** of the `DISPLAY_ENDPOINT` protocol
//! over injected seams, so the semantics — what a request means, what is
//! validated, what is refused — have exactly one definition:
//!
//! * [`server::DisplayServer`] — the engine a display driver's `Run`
//!   binary hosts: decode → live-lease check ([`server::SeatCheck`]) →
//!   geometry/bounds validation → blit through the
//!   [`Display`](tairix_abi::driver::display::Display) trait.
//! * [`client::DisplayClient`] / [`client::RemoteDisplay`] — the
//!   session-side half over a [`client::DisplayTransport`]:
//!   `RemoteDisplay` implements the existing `Display` trait over the
//!   client's mapping of the shared frame region, so a compositor
//!   presents through it unchanged.
//!
//! The wire format itself lives in `tairix_abi::display_ipc`; this crate
//! adds the behaviour. Frames travel through one `shm_grant`ed region
//! mapped once at configure time — presents carry a frame index and a
//! damage rectangle, never pixels.
//!
//! The crate also hosts [`framebuffer::Framebuffer`] — the generic
//! linear-surface engine the framebuffer service's `Run` binary scans
//! out through (and the framebuffer QEMU verticals drive directly), so
//! the surface blit has exactly one definition.

#![no_std]
// The engine is unsafe-free by construction; only the feature-gated `rt`
// module's single audited mapping view (its `SAFETY` block) is exempt, so
// the crate keeps the outright forbid whenever that module is not built.
#![cfg_attr(not(feature = "rt"), forbid(unsafe_code))]
#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod client;
pub mod framebuffer;
#[cfg(feature = "rt")]
pub mod rt;
pub mod server;

pub use client::{DisplayClient, DisplayTransport, RemoteDisplay};
pub use framebuffer::{Framebuffer, FramebufferConfig};
#[cfg(feature = "rt")]
pub use rt::{RtShmMapper, RtShmRegion};
pub use server::{DisplayServer, FrameRegion, SeatCheck, ShmMapper, DISPLAY_REPLY_MAX};

use tairix_abi::{DriverError, Errno};

/// Map a typed [`Errno`] a display-service reply carried back onto the
/// [`DriverError`] the `Display` trait's callers expect — the inverse
/// boundary conversion for [`client::RemoteDisplay`], defined once.
///
/// The seat refusals keep their meaning ([`Errno::SeatRevoked`] is the
/// distinct "you lost the seat" signal a compositor tears down on;
/// [`Errno::SeatNotOwner`] and every other authority refusal surface as
/// `PermissionDenied`, exactly as the kernel-side present gate reports
/// them). Conditions with no driver-level equivalent fail closed as
/// `DeviceFault`: the remote display is the device, and an
/// uninterpretable refusal from it must read as "unhealthy", never as
/// success.
#[must_use]
pub fn driver_error_from_errno(err: Errno) -> DriverError {
    match err {
        Errno::BufferTooSmall => DriverError::BufferTooSmall,
        Errno::BadMagic => DriverError::BadMagic,
        Errno::AbiVersionUnsupported => DriverError::AbiVersionUnsupported,
        Errno::LengthOutOfRange => DriverError::LengthOutOfRange,
        Errno::OutOfRange => DriverError::OutOfRange,
        Errno::PermissionDenied | Errno::SeatNotOwner => DriverError::PermissionDenied,
        Errno::NotFound => DriverError::NotFound,
        Errno::SignatureInvalid => DriverError::SignatureInvalid,
        Errno::NotImplemented => DriverError::NotImplemented,
        Errno::WouldBlock => DriverError::Busy,
        Errno::NoSpace => DriverError::NoSpace,
        Errno::SeatRevoked => DriverError::SeatRevoked,
        Errno::EndpointStalled => DriverError::EndpointStalled,
        // `Errno::DeviceFault` maps to itself; every other condition has
        // no driver-level equivalent and fails closed as a fault too.
        _ => DriverError::DeviceFault,
    }
}

#[cfg(test)]
mod tests;
