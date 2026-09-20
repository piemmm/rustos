//! The driver-side per-request handler of the `audiochan-v1` device-channel
//! contract (`plans/SOUND.md` SND4).
//!
//! [`AudioChannelServer`] is the pure, host-testable engine an audio driver
//! *process* drives: the process owns the device (MMIO/DMA/IRQ) and the call
//! endpoint, and hands each decoded
//! [`AudioChannelRequest`](tairix_abi::driver::audio_channel::AudioChannelRequest)
//! to this server, which turns it into the right device action and the
//! matching reply. The I/O — receiving the request, mapping the granted PCM
//! regions, sending the reply and the period notifies — stays in the crate's
//! `serve` loop.
//!
//! # State machine, per endpoint
//!
//! A device presents several sinks and sources and each is driven
//! independently, so the state is per endpoint rather than per channel.
//!
//! An endpoint starts **unconfigured**: `Facts` and `EndpointFacts` answer,
//! and everything else refuses with [`Errno::NotConnected`]. `Configure`
//! programs the hardware and records the grant the ring will be shaped from;
//! `Attach` validates the offered geometry against *that recorded grant* and
//! moves the endpoint to **attached**, from which `Start`, `Stop`, `Drain`
//! and `Service` work. `Detach` releases the endpoint's device-side resources
//! and returns it to unconfigured.
//!
//! Validating the attach against the recorded grant is what stops the two
//! sides disagreeing about the region's size: the grant is the *device's* own
//! answer, and [`ConfigureGrant::geometry`] is the single derivation both
//! sides size the region from.
//!
//! # Fail closed
//!
//! Every reply is a fully-encoded `audiochan-v1` frame. An endpoint index the
//! device does not present, a service before attach, a region whose length
//! does not match the agreed geometry, or any device fault is a typed
//! [`Errno`] carried in the reply's status word — never a panic, never a
//! partially-applied action.

use tairix_abi::driver::audio::{
    Audio, AudioServiced, Frames, MAX_DEVICE_ENDPOINTS, MAX_SIGNALLED_ENDPOINTS,
};
use tairix_abi::driver::audio_channel::{
    encode_configure_reply, encode_endpoint_reply, encode_facts_reply, encode_service_reply,
    AttachParams, AudioServiceReport, ConfigureGrant, ConfigureParams,
    AUDIO_CHANNEL_CONFIGURE_REPLY_LEN, AUDIO_CHANNEL_ENDPOINT_REPLY_LEN,
    AUDIO_CHANNEL_FACTS_REPLY_LEN, AUDIO_CHANNEL_SERVICE_REPLY_LEN,
};
use tairix_abi::driver::audio_ring::{PcmGeometry, PcmRing};
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::{DriverError, Errno};

/// Endpoint slots the server tracks.
///
/// The ABI's own endpoint ceiling, so a device may not present an endpoint
/// this server cannot hold state for. The interrupt bitmaps are the same
/// width, which is what lets one word name every endpoint of a device.
const ENDPOINT_SLOTS: usize = MAX_DEVICE_ENDPOINTS as usize;

const _: () = assert!(ENDPOINT_SLOTS <= MAX_SIGNALLED_ENDPOINTS as usize);

/// What one serviced period moved, and what the mixer must be told about it.
///
/// The driver's interrupt path needs the report as a *value* — it services
/// the ring itself and then sends a notify — so the service work is exposed
/// beside the reply-encoding form. `lost_frames` is the delta this call
/// added to the endpoint's cumulative under/over-run tally, because the
/// notify reports the loss that just happened while the report carries the
/// running total.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Serviced {
    /// What the device engine reported.
    pub report: AudioServiced,
    /// Frames lost since the previous service of this endpoint.
    pub lost_frames: u64,
}

/// One endpoint's configured state.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Configured {
    /// What the device answered it would actually run at; the ring's shape is
    /// derived from this and nothing else.
    grant: ConfigureGrant,
    /// The attached region, once the mixer has granted one.
    attached: Option<Attached>,
    /// The cumulative under/over-run tally the previous service reported, so
    /// the interrupt path can report the *delta* a notify carries.
    seen_xrun_frames: u64,
}

/// One endpoint's attached shared region.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Attached {
    /// The ring geometry both sides agreed, derived once through
    /// [`ConfigureGrant::geometry`]; every [`PcmRing`] view binds with
    /// exactly this.
    geometry: PcmGeometry,
    /// Numeric IPC endpoint the driver `ipc_send`s this endpoint's notifies
    /// to (the mixer bound and owns it).
    notify_endpoint: u64,
}

/// The driver-side handler of one audio device channel.
///
/// Wraps the concrete [`Audio`] device engine and tracks each endpoint's
/// configuration and attach state. It never performs I/O: the process binary
/// receives the request, maps the regions, and sends the replies this server
/// produces.
pub struct AudioChannelServer<A: Audio> {
    audio: A,
    endpoints: [Option<Configured>; ENDPOINT_SLOTS],
}

impl<A: Audio> AudioChannelServer<A> {
    /// Build a server over the device engine `audio`, with every endpoint
    /// unconfigured.
    pub fn new(audio: A) -> Self {
        Self {
            audio,
            endpoints: [None; ENDPOINT_SLOTS],
        }
    }

    /// Borrow the underlying device engine (host-test access).
    #[must_use]
    pub fn audio(&self) -> &A {
        &self.audio
    }

    /// Borrow the underlying device engine mutably.
    ///
    /// The driver process needs this to mask and unmask the device's event
    /// sources and to read its interrupt causes — the interrupt-side work
    /// that is I/O and so lives in the serve loop rather than in this pure
    /// server.
    pub fn audio_mut(&mut self) -> &mut A {
        &mut self.audio
    }

    /// The numeric IPC endpoint this endpoint's notifies go to, or [`None`]
    /// while it is unconfigured or unattached.
    #[must_use]
    pub fn notify_endpoint(&self, endpoint: u16) -> Option<u64> {
        self.slot(endpoint)?
            .as_ref()?
            .attached
            .as_ref()
            .map(|a| a.notify_endpoint)
    }

    /// The agreed ring geometry for `endpoint`, or [`None`] while it is not
    /// attached. The process binary sizes the region it maps from
    /// [`PcmGeometry::region_len`].
    #[must_use]
    pub fn geometry(&self, endpoint: u16) -> Option<PcmGeometry> {
        self.slot(endpoint)?
            .as_ref()?
            .attached
            .as_ref()
            .map(|a| a.geometry)
    }

    /// Whether `endpoint` has a shared region attached.
    #[must_use]
    pub fn is_attached(&self, endpoint: u16) -> bool {
        self.geometry(endpoint).is_some()
    }

    /// Answer `Facts`: the device's own facts, or a `-errno` status on a
    /// device fault.
    #[must_use]
    pub fn facts_reply(&self) -> [u8; AUDIO_CHANNEL_FACTS_REPLY_LEN] {
        encode_facts_reply(self.audio.device_facts().map_err(DriverError::as_errno))
    }

    /// Answer `EndpointFacts`: what one sink or source can do.
    #[must_use]
    pub fn endpoint_facts_reply(&self, endpoint: u16) -> [u8; AUDIO_CHANNEL_ENDPOINT_REPLY_LEN] {
        encode_endpoint_reply(
            self.audio
                .endpoint_facts(endpoint)
                .map_err(DriverError::as_errno),
        )
    }

    /// Answer `Configure`: program the endpoint and record the grant the ring
    /// will be shaped from.
    ///
    /// A reconfiguration discards any attached region: the region's size
    /// follows the grant, so a grant that changed leaves the old mapping the
    /// wrong shape. The process binary observes that through
    /// [`is_attached`](Self::is_attached) and unmaps accordingly, so a stale
    /// mapping is never serviced.
    #[must_use]
    pub fn configure_reply(
        &mut self,
        params: &ConfigureParams,
    ) -> [u8; AUDIO_CHANNEL_CONFIGURE_REPLY_LEN] {
        encode_configure_reply(self.configure(params))
    }

    fn configure(&mut self, params: &ConfigureParams) -> Result<ConfigureGrant, Errno> {
        let slot = Self::slot_index(params.endpoint)?;
        let grant = self
            .audio
            .configure(params.endpoint, params)
            .map_err(DriverError::as_errno)?;
        // A grant the mixer's own decoder would refuse must never be
        // recorded here: the two sides would then disagree about whether the
        // endpoint is configured at all. One definition, applied on both
        // sides of the wire.
        grant.validate().map_err(|_| Errno::DeviceFault)?;
        self.endpoints[slot] = Some(Configured {
            grant,
            attached: None,
            seen_xrun_frames: 0,
        });
        Ok(grant)
    }

    /// Answer `Attach`: validate the offered ring against the recorded grant
    /// and, on success, move the endpoint to attached.
    ///
    /// The process binary maps the granted region *before* calling this and
    /// unmaps it again if the server refuses, so a rejected attach never
    /// half-binds.
    #[must_use]
    pub fn attach(&mut self, params: &AttachParams) -> [u8; STATUS_REPLY_LEN] {
        encode_status_reply(self.try_attach(params))
    }

    fn try_attach(&mut self, params: &AttachParams) -> Result<(), Errno> {
        let slot = Self::slot_index(params.endpoint)?;
        let configured = self.endpoints[slot].as_mut().ok_or(Errno::NotConnected)?;
        // The one derivation: the mixer created a region of exactly this many
        // bytes, so a ring the device's own grant does not admit is refused
        // here rather than mis-read on the period path.
        let geometry = configured.grant.geometry(params.ring_frames)?;
        configured.attached = Some(Attached {
            geometry,
            notify_endpoint: params.notify_endpoint,
        });
        Ok(())
    }

    /// Answer `Start`: begin clocking the endpoint at an exact position.
    #[must_use]
    pub fn start(&mut self, endpoint: u16, at: Frames) -> [u8; STATUS_REPLY_LEN] {
        encode_status_reply(self.transport(endpoint, |audio| audio.start(endpoint, at)))
    }

    /// Answer `Stop`: stop clocking the endpoint at an exact position.
    #[must_use]
    pub fn stop(&mut self, endpoint: u16, at: Frames) -> [u8; STATUS_REPLY_LEN] {
        encode_status_reply(self.transport(endpoint, |audio| audio.stop(endpoint, at)))
    }

    /// Answer `Drain`: clock out what is queued, then stop.
    #[must_use]
    pub fn drain(&mut self, endpoint: u16) -> [u8; STATUS_REPLY_LEN] {
        encode_status_reply(self.transport(endpoint, |audio| audio.drain(endpoint)))
    }

    /// Run a transport action that requires an attached endpoint.
    ///
    /// Starting an endpoint with no region would clock a device out of memory
    /// nothing owns, so the attach check comes before the device is touched.
    fn transport<F>(&mut self, endpoint: u16, action: F) -> Result<(), Errno>
    where
        F: FnOnce(&mut A) -> Result<(), DriverError>,
    {
        let slot = Self::slot_index(endpoint)?;
        let configured = self.endpoints[slot].as_ref().ok_or(Errno::NotConnected)?;
        if configured.attached.is_none() {
            return Err(Errno::NotConnected);
        }
        action(&mut self.audio).map_err(DriverError::as_errno)
    }

    /// Answer `Gain`: set the endpoint's hardware gain and mute.
    ///
    /// Accepted whether or not the endpoint is attached: gain is device
    /// state, not channel state, so the mixer may program it before frames
    /// flow. A device with no gain control refuses, which is how the mixer
    /// learns to apply the gain in software instead.
    #[must_use]
    pub fn set_gain(&mut self, endpoint: u16, millibel: i32, mute: bool) -> [u8; STATUS_REPLY_LEN] {
        encode_status_reply(Self::slot_index(endpoint).and_then(|_| {
            self.audio
                .set_gain(endpoint, millibel, mute)
                .map_err(DriverError::as_errno)
        }))
    }

    /// Answer `Service`: bind a ring view over the caller-mapped `region` and
    /// move one period between it and the device.
    #[must_use]
    pub fn service_reply(
        &mut self,
        endpoint: u16,
        region: &mut [u8],
    ) -> [u8; AUDIO_CHANNEL_SERVICE_REPLY_LEN] {
        encode_service_reply(self.service(endpoint, region).map(|serviced| {
            let report = serviced.report;
            AudioServiceReport {
                transferred: report.transferred,
                running: report.running,
                position: report.position,
                xrun_frames: report.xrun_frames,
                sampled_at: report.sampled_at,
            }
        }))
    }

    /// Move one period between `region` and the device, and report what
    /// moved.
    ///
    /// The same work [`service_reply`](Self::service_reply) encodes, for the
    /// driver's own interrupt path: a period interrupt refills the device
    /// straight from the shared region rather than waiting to be asked, so
    /// the report is needed as a value rather than as reply bytes.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] for an endpoint the device does not present,
    /// [`Errno::NotConnected`] before attach, [`Errno::BufferTooSmall`] or
    /// [`Errno::BadAlignment`] for a region that does not match the agreed
    /// geometry, or the device's typed fault.
    pub fn service(&mut self, endpoint: u16, region: &mut [u8]) -> Result<Serviced, Errno> {
        let slot = Self::slot_index(endpoint)?;
        let configured = self.endpoints[slot].as_mut().ok_or(Errno::NotConnected)?;
        let geometry = configured
            .attached
            .as_ref()
            .ok_or(Errno::NotConnected)?
            .geometry;
        let mut ring = PcmRing::bind(region, geometry)?;
        let report = self
            .audio
            .service(endpoint, &mut ring)
            .map_err(DriverError::as_errno)?;
        // Saturating rather than wrapping: a device that reported a tally
        // going backwards is reporting nonsense, and a vast phantom loss is a
        // worse answer than none.
        let lost_frames = report
            .xrun_frames
            .saturating_sub(configured.seen_xrun_frames);
        configured.seen_xrun_frames = report.xrun_frames;
        Ok(Serviced {
            report,
            lost_frames,
        })
    }

    /// Answer `Detach`: release the endpoint's device-side resources and
    /// forget its configuration.
    ///
    /// The process binary unmaps the region afterwards. The device is
    /// released rather than merely forgotten, because a device left
    /// configured against a region nobody maps would keep clocking into
    /// memory the driver no longer owns.
    #[must_use]
    pub fn detach(&mut self, endpoint: u16) -> [u8; STATUS_REPLY_LEN] {
        encode_status_reply(Self::slot_index(endpoint).and_then(|slot| {
            let released = self.audio.release(endpoint);
            // Forgotten whichever way the device answered: a release the
            // hardware refused still leaves this channel with no region, and
            // keeping the state would let a later `Service` bind a mapping
            // the process is about to drop.
            self.endpoints[slot] = None;
            released.map_err(DriverError::as_errno)
        }))
    }

    /// Whether any endpoint has a region attached — the serve loop's test for
    /// "is there anywhere to put frames at all".
    #[must_use]
    pub fn any_attached(&self) -> bool {
        self.endpoints
            .iter()
            .any(|slot| slot.as_ref().is_some_and(|c| c.attached.is_some()))
    }

    fn slot(&self, endpoint: u16) -> Option<&Option<Configured>> {
        self.endpoints.get(usize::from(endpoint))
    }

    /// Resolve an endpoint index onto its slot, refusing one the contract
    /// does not admit.
    fn slot_index(endpoint: u16) -> Result<usize, Errno> {
        if endpoint >= MAX_DEVICE_ENDPOINTS {
            return Err(Errno::NotFound);
        }
        Ok(usize::from(endpoint))
    }
}

#[cfg(test)]
mod tests;
