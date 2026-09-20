//! Handing a discovered sound device's channel to the audio service.
//!
//! An audio driver process, once the device manager has autoloaded it for a
//! matched audio node, brings its device online and publishes a child
//! *device-channel* hardware-tree node: `compatible = "tairix,audiochan"`,
//! carrying the reserved call-endpoint id it bound as an
//! [`HwResourceKind::Endpoint`] grant request. Emitting that node bumps the
//! hardware-tree generation, waking the device manager's reactive loop.
//!
//! This module is the pure policy for that reaction, the audio twin of
//! [`netbind`](crate::netbind): recognise an `audiochan` node, read its
//! endpoint, and — for each channel not already handed over — ask the audio
//! service to adopt it, over the [`AudiodBind`] seam so the loop stays
//! host-testable. The service becomes the channel's client, enumerates the
//! device's sinks and sources, and owns every shared PCM region from there;
//! the device manager only names *which* endpoint.
//!
//! Each endpoint is handed over exactly once (tracked in [`AudioBindState`]):
//! the node persists across every later generation bump while the driver
//! lives, so a re-bind would provision a duplicate device. A hand-off that
//! fails (the service is not up yet, or refuses) is fail-soft — logged and
//! retried on the next bump, exactly like an unavailable driver store — never
//! fatal to the observe loop.

use alloc::collections::BTreeSet;

use tairix_abi::driver::audio_channel::AUDIOCHAN_NODE_COMPATIBLE;
use tairix_abi::hwtree::{HwMatchKind, HwResourceKind};
use tairix_abi::{Errno, HwNode};
use tairix_log::{log as log_event, Event, EventId, Field, FieldValue, Level, Sink};

use crate::events;

/// The device manager's call into the audio service to adopt one audio
/// driver's device channel.
///
/// The production implementation (the freestanding `devmgr` `Run` binary)
/// backs this with an `ipc_call` to the reserved
/// [`AUDIO_ENDPOINT`](tairix_abi::audio::AUDIO_ENDPOINT) carrying an
/// [`AudioRequest::BindDriver`](tairix_abi::audio::AudioRequest::BindDriver);
/// the audio service checks the call against the device manager's attested
/// `CAP_DRV_LOAD`, so the seam adds no authority. It is abstracted here so
/// the reactive loop is host-testable against a recording double.
pub trait AudiodBind {
    /// Ask the audio service to adopt the driver's device-channel
    /// `endpoint_id` as a sound device.
    ///
    /// # Errors
    ///
    /// The service's typed refusal, or a transport failure — treated
    /// fail-soft by the caller (retried on the next generation bump).
    fn bind_driver(&mut self, endpoint_id: u64) -> Result<(), Errno>;
}

/// The device manager's memory of which audio channels it has already handed
/// to the audio service.
///
/// An `audiochan` node persists across every generation bump for as long as
/// its driver lives, so binding is idempotent: an endpoint already handed
/// over is skipped.
#[derive(Default)]
pub struct AudioBindState {
    bound: BTreeSet<u64>,
}

impl AudioBindState {
    /// A fresh state with nothing bound.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the channel `endpoint_id` has already been handed over.
    #[must_use]
    pub fn is_bound(&self, endpoint_id: u64) -> bool {
        self.bound.contains(&endpoint_id)
    }
}

/// If `node` is an audio device-channel node — its match keys carry the
/// [`AUDIOCHAN_NODE_COMPATIBLE`] `compatible` string — return the
/// call-endpoint id it published as an [`HwResourceKind::Endpoint`] grant
/// request.
///
/// Returns [`None`] for any other node, and for an `audiochan` node that
/// carries no endpoint resource (a malformed emission — never guessed at).
#[must_use]
pub fn audiochan_endpoint(node: &HwNode) -> Option<u64> {
    let is_audiochan = node.match_keys().iter().any(|key| {
        key.kind() == Some(HwMatchKind::Compatible)
            && key.compatible_bytes() == AUDIOCHAN_NODE_COMPATIBLE
    });
    if !is_audiochan {
        return None;
    }
    node.resources()
        .iter()
        .find(|resource| resource.kind() == Some(HwResourceKind::Endpoint))
        .map(tairix_abi::HwResource::base)
}

/// Hand every not-yet-adopted audio device channel in `nodes` to the audio
/// service through `audiod`, recording each success in `state`.
///
/// An endpoint already in `state` is skipped (idempotent across generation
/// bumps). A hand-off the service refuses is fail-soft: logged and left for
/// the next bump to retry (the service may not have claimed its rendezvous
/// yet), never fatal to the observe loop.
pub fn bind_new_channels(
    nodes: &[HwNode],
    state: &mut AudioBindState,
    audiod: &mut dyn AudiodBind,
    sink: &dyn Sink,
) {
    for node in nodes {
        let Some(endpoint) = audiochan_endpoint(node) else {
            continue;
        };
        if state.bound.contains(&endpoint) {
            continue;
        }
        match audiod.bind_driver(endpoint) {
            Ok(()) => {
                state.bound.insert(endpoint);
                audit(
                    sink,
                    events::AUDIOD_BOUND,
                    Level::Info,
                    "audiochan device channel bound to audio service",
                    endpoint,
                    None,
                );
            }
            Err(err) => audit(
                sink,
                events::AUDIOD_BIND_FAILED,
                Level::Warn,
                "audiochan device-channel bind to audio service failed; will retry",
                endpoint,
                Some(err),
            ),
        }
    }
}

/// Emit one audit record carrying the channel endpoint the decision was
/// about, so an operator can correlate it with the driver that published it,
/// and — on a refusal — what the service actually said, without which the
/// record names a failure but not its cause.
fn audit(
    sink: &dyn Sink,
    id: EventId,
    level: Level,
    message: &'static str,
    endpoint: u64,
    error: Option<Errno>,
) {
    let endpoint = Field {
        key: "endpoint",
        value: FieldValue::UnsignedInt(endpoint),
    };
    let fields = match error {
        Some(err) => &[
            endpoint,
            Field {
                key: "error",
                value: FieldValue::Error(err),
            },
        ][..],
        None => &[endpoint][..],
    };
    log_event(
        sink,
        &Event {
            level,
            id,
            message,
            fields,
        },
    );
}

#[cfg(test)]
#[path = "audiobind_tests.rs"]
mod tests;
