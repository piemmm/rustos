//! Generic firmware property-mailbox channel seam (`abi-v1`).
//!
//! [`MailboxChannel`] is the board-neutral seam a host exposes
//! ([`DriverHost::mailbox`](super::DriverHost::mailbox)) so a driver can run a
//! firmware property-channel exchange without knowing where the doorbell
//! registers live, how the property buffer is carved, or how CPU-physical
//! addresses alias onto the device's bus aperture. All of those board
//! specifics stay behind the device's own crate (`lib/vcmailbox`, the
//! BCM2711 `VideoCore` client carve-out); the host hands
//! the driver a ready channel and the driver only marshals 32-word property
//! messages through it.
//!
//! The trait lives in `lib/abi` (not in `lib/vcmailbox`) so the host
//! accessor can name it without inverting the dependency direction: `lib/vcmailbox` depends on `lib/abi`, never the reverse.
//! `lib/vcmailbox` adapts its low-level `MailboxTransport` (which is
//! `&mut self`, for its own per-exchange bookkeeping) onto this `&self`
//! channel; the host serialises concurrent callers behind its own lock so the
//! shared-reference shape is sound.

use crate::DriverError;

/// Number of 32-bit words in a `VideoCore` property-channel message.
///
/// The property channel exchanges a fixed-size, word-aligned buffer: a header
/// (buffer size, request/response code), a sequence of property tags, and an
/// end marker. Both the encoder/decoder in `lib/vcmailbox` and this seam use
/// the same width so the buffer shape is defined exactly once.
pub const MAILBOX_PROPERTY_WORDS: usize = 32;

/// Host-side seam for a firmware property-mailbox exchange.
///
/// A driver obtains a `&dyn MailboxChannel` from
/// [`DriverHost::mailbox`](super::DriverHost::mailbox) and calls
/// [`exchange`](Self::exchange) with an encoded property message; on success
/// the same buffer holds the firmware's decoded response. The channel is the
/// *only* thing the driver needs — the doorbell window, the DMA-backed
/// property buffer, the bus-address translation, and any cache maintenance
/// are owned by the host's concrete implementation, keeping the driver free
/// of board addresses and of any `kernel/*` dependency.
pub trait MailboxChannel {
    /// Run one property-channel exchange.
    ///
    /// `message` is a [`MAILBOX_PROPERTY_WORDS`]-word property buffer encoded
    /// by the caller (e.g. via `lib/vcmailbox`). On `Ok` the buffer is
    /// overwritten in place with the firmware's response for the caller to
    /// decode; on `Err` the buffer contents are unspecified and the caller
    /// must treat the exchange as failed (fail closed).
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if the host's capability check for
    ///   the underlying doorbell/buffer access fails.
    /// * [`DriverError::DeviceFault`] if the firmware reports an error code or
    ///   the doorbell handshake times out.
    /// * [`DriverError::Unsupported`] if no mailbox is wired on this platform
    ///   (the host should instead report [`None`] from
    ///   [`DriverHost::mailbox`](super::DriverHost::mailbox); see its docs).
    ///
    /// # Capabilities
    ///
    /// None at the call site; the host enforces the capability gate for the
    /// doorbell MMIO and property-buffer DMA it owns.
    fn exchange(&self, message: &mut [u32; MAILBOX_PROPERTY_WORDS]) -> Result<(), DriverError>;
}
