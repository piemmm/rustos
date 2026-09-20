//! Stable audit event ids the audio service records its security decisions
//! under (`plans/SOUND.md`, `plans/SYSLOG.md`).
//!
//! A refusal is as audit-worthy as a grant — "who tried" is the question an
//! incident asks — so the capture path records both. The recording indicator
//! the session draws is rendered from the same live state these records
//! describe, so what a user sees and what the log holds cannot disagree.

use tairix_log::EventId;

/// A driver's device channel was bound and its endpoints enumerated.
pub const DEVICE_BOUND: EventId = EventId(4220);

/// A device channel could not be bound: its facts were unreadable, or it
/// presented no endpoint this service could serve.
pub const DEVICE_BIND_FAILED: EventId = EventId(4221);

/// A device went away mid-stream. Its streams hold their positions and are
/// told; every other device is untouched.
pub const DEVICE_LOST: EventId = EventId(4222);

/// The machine's default sink or source changed.
pub const DEFAULT_DEVICE_CHANGED: EventId = EventId(4223);

/// A capture stream was opened, and by which principal.
pub const CAPTURE_OPENED: EventId = EventId(4224);

/// A capture stream was refused, and to which principal. Recorded whether
/// the refusal was the missing capability, an unknown source, or a device
/// that could not be configured.
pub const CAPTURE_REFUSED: EventId = EventId(4225);

/// A capture stream closed, so the live-capture set — and the indicator
/// drawn from it — shrank.
pub const CAPTURE_CLOSED: EventId = EventId(4226);

/// The service claimed its `audio-v1` rendezvous and is serving.
pub const AUDIOD_READY: EventId = EventId(4227);

/// A stream open the service refused, carrying what it refused with.
///
/// Capture refusals also raise [`CAPTURE_REFUSED`], which is the
/// security-relevant record; this one is the diagnosis, and covers playback
/// too — without it a machine whose sound does not work says nothing at all.
pub const STREAM_REFUSED: EventId = EventId(4228);
