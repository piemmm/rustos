//! The routing policy: a pure function from (role, who holds the sink, which
//! sink was asked for) to what happens to a stream.
//!
//! This is where "policy, not a per-application configuration file" is cashed
//! in. A program says what its sound is *for* and nothing else; which sink it
//! lands on, whether it survives a user switch, and whether it ducks are all
//! decided here, from state, by one function with no I/O and no process
//! names in it. That is what makes the behaviour testable over the whole
//! cross-product rather than discoverable by experiment.
//!
//! # The seat owns the sound exactly as it owns the screen
//!
//! A sink may be leased to a session. A stream whose owner holds that lease
//! is mixed; one whose owner does not is **paused at a frame boundary and
//! told so**, holding its position so a switch back resumes on the frame it
//! stopped on. A departing user's music does not play into the arriving
//! user's room, and it does not silently vanish either.
//!
//! A notification is the exception, and its role is what says so: one that
//! arrives ten minutes late is noise, so it is dropped rather than queued.
//!
//! # An unleased sink is anybody's
//!
//! A machine with no graphical session — a server playing an alert — has no
//! lease on its sinks, and a stream there simply plays. On a machine with a
//! session, the session claims its seat's sinks at login, which is what stops
//! a remote login making noise in somebody's room.

use tairix_abi::audio::StreamRole;
use tairix_abi::Errno;
use tairix_seat::SeatOwner;

use crate::volume::DUCK_MILLIBEL;

/// One sink, as the policy sees it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SinkState {
    /// The service-assigned identity a stream is opened against.
    pub device_id: u32,
    /// Whether the machine's configured default for playback.
    pub is_default: bool,
    /// The session holding this sink's seat, where one does.
    pub leased_to: Option<SeatOwner>,
}

/// One stream, as the policy sees it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct StreamRequest {
    /// What the sound is for.
    pub role: StreamRole,
    /// The sink the client named, or [`None`] for the machine default.
    pub requested_device: Option<u32>,
    /// The session the stream belongs to, or [`None`] for a principal with no
    /// session at all — a daemon, or a login with no seat.
    pub owner: Option<SeatOwner>,
}

/// What the policy decided.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Routing {
    /// Mix the stream into this sink.
    Play {
        /// The sink it lands on.
        device_id: u32,
    },
    /// Hold the stream at its frame position and tell it why: its session does
    /// not hold the sink's lease.
    Pause {
        /// The sink it would land on when its session is next active.
        device_id: u32,
    },
    /// Discard the stream's frames. Only a notification from an inactive
    /// session, whose lateness would make it noise.
    Drop,
    /// Refuse the stream outright.
    Refuse(
        /// Why.
        Errno,
    ),
}

/// Decide what happens to `request` given the sinks the machine has.
///
/// Fails closed: a named sink that does not exist, and a machine with no
/// configured default, are refusals rather than a sink picked arbitrarily.
#[must_use]
pub fn route(request: &StreamRequest, sinks: &[SinkState]) -> Routing {
    let Some(sink) = choose(request.requested_device, sinks) else {
        return Routing::Refuse(match request.requested_device {
            // A named device that is not there is a different answer from a
            // machine that has not been told which sink to use.
            Some(_) => Errno::NotFound,
            None => Errno::DeviceOffline,
        });
    };
    match sink.leased_to {
        // Nobody has claimed the room, so anybody may play in it.
        None => Routing::Play {
            device_id: sink.device_id,
        },
        Some(holder) if request.owner == Some(holder) => Routing::Play {
            device_id: sink.device_id,
        },
        Some(_) if request.role == StreamRole::Notification => Routing::Drop,
        Some(_) => Routing::Pause {
            device_id: sink.device_id,
        },
    }
}

/// The sink a request lands on: the one it named, else the configured
/// default.
fn choose(requested: Option<u32>, sinks: &[SinkState]) -> Option<&SinkState> {
    match requested {
        Some(device_id) => sinks.iter().find(|sink| sink.device_id == device_id),
        None => sinks.iter().find(|sink| sink.is_default),
    }
}

/// The attenuation `role` takes while `others` are live on the same sink.
///
/// Media steps aside for speech — a conversation or assistive output — and
/// nothing else ducks: a notification is short enough that attenuating the
/// music under it would be more noticeable than the notification, and two
/// conversations on one sink are the user's own doing.
#[must_use]
pub fn duck_millibel(role: StreamRole, others: &[StreamRole]) -> i32 {
    if role != StreamRole::Media {
        return 0;
    }
    let speech = others
        .iter()
        .any(|other| matches!(other, StreamRole::Communication | StreamRole::Accessibility));
    if speech {
        DUCK_MILLIBEL
    } else {
        0
    }
}

#[cfg(test)]
#[path = "route_tests.rs"]
mod tests;
