//! The mixer-side client of a driver process's `audiochan-v1` endpoint
//! (`plans/SOUND.md` SND4).
//!
//! The driver is the channel's *server* (`lib/audiochan`) and this service is
//! its one client — the kernel binds the driver's endpoint restricted-sender
//! on `CAP_AUDIO_DEVICE`, so it refuses at dispatch every caller but this
//! one. Each control operation is one `ipc_call` over the injected
//! [`AudioChannelTransport`], which is what keeps the client pure and
//! host-testable against an in-process `AudioChannelServer`.
//!
//! The doorbell is deliberately *not* how frames move on the steady path: the
//! driver services its own ring from the device's period interrupt and then
//! sends the clock pair, so the mixer's job on waking is to refill. A
//! [`service`](AudioChannelClient::service) call is what primes a ring before
//! `Start` and what re-arms a driver whose event sources went down for
//! back-pressure.

use tairix_abi::driver::audio::{AudioDeviceFacts, AudioEndpointFacts, Frames};
use tairix_abi::driver::audio_channel::{
    decode_configure_reply, decode_endpoint_reply, decode_facts_reply, decode_service_reply,
    AttachParams, AudioChannelRequest, AudioServiceReport, ConfigureGrant, ConfigureParams,
    AUDIO_CHANNEL_MAX_REPLY, AUDIO_CHANNEL_MAX_REQUEST,
};
use tairix_abi::reply::decode_status_reply;
use tairix_abi::Errno;

/// The injected call transport of an [`AudioChannelClient`].
///
/// One `ipc_call` to the driver process's device endpoint: the live service
/// backs it with `tairix_rt::ipc_call` (a bare-metal-only dependency) and
/// host tests back it with an in-process fake dispatching to a
/// `tairix_audiochan::AudioChannelServer`, so the client is exercised without
/// a kernel.
pub trait AudioChannelTransport {
    /// Send `request` to the driver endpoint and copy its reply into
    /// `reply`, returning the reply length.
    ///
    /// # Errors
    ///
    /// The transport's typed [`Errno`] — a destroyed endpoint, an oversize
    /// message, or a reply larger than `reply`.
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno>;
}

impl AudioChannelTransport for alloc::boxed::Box<dyn AudioChannelTransport> {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        (**self).call(request, reply)
    }
}

/// The mixer-side client of one audio device channel.
pub struct AudioChannelClient<T: AudioChannelTransport> {
    transport: T,
}

impl<T: AudioChannelTransport> AudioChannelClient<T> {
    /// Wrap `transport` as a device-channel client.
    pub const fn new(transport: T) -> Self {
        Self { transport }
    }

    /// What the device is, and how many endpoints it presents.
    ///
    /// # Errors
    ///
    /// A transport failure, or the driver's typed refusal.
    pub fn facts(&mut self) -> Result<AudioDeviceFacts, Errno> {
        self.exchange(AudioChannelRequest::Facts, decode_facts_reply)
    }

    /// What one sink or source of the device can do.
    ///
    /// # Errors
    ///
    /// A transport failure, or the driver's typed refusal (an index it does
    /// not present).
    pub fn endpoint_facts(&mut self, endpoint: u16) -> Result<AudioEndpointFacts, Errno> {
        self.exchange(
            AudioChannelRequest::EndpointFacts { endpoint },
            decode_endpoint_reply,
        )
    }

    /// Program an endpoint, and be told what the device could actually meet.
    ///
    /// # Errors
    ///
    /// A transport failure, or the driver's typed refusal. The reply's own
    /// decoder applies [`ConfigureGrant::validate`], so a grant no ring could
    /// be built from never reaches the caller.
    pub fn configure(&mut self, params: ConfigureParams) -> Result<ConfigureGrant, Errno> {
        self.exchange(
            AudioChannelRequest::Configure(params),
            decode_configure_reply,
        )
    }

    /// Hand the granted device ring over and name the notify port.
    ///
    /// # Errors
    ///
    /// A transport failure, or the driver's typed refusal (a region its own
    /// grant does not admit).
    pub fn attach(&mut self, params: AttachParams) -> Result<(), Errno> {
        self.exchange(AudioChannelRequest::Attach(params), decode_status_reply)
    }

    /// Begin clocking `endpoint` at an exact frame position.
    ///
    /// # Errors
    ///
    /// A transport failure, or the driver's typed refusal.
    pub fn start(&mut self, endpoint: u16, at: Frames) -> Result<(), Errno> {
        self.exchange(
            AudioChannelRequest::Start { endpoint, at },
            decode_status_reply,
        )
    }

    /// Stop clocking `endpoint` at an exact frame position.
    ///
    /// # Errors
    ///
    /// A transport failure, or the driver's typed refusal.
    pub fn stop(&mut self, endpoint: u16, at: Frames) -> Result<(), Errno> {
        self.exchange(
            AudioChannelRequest::Stop { endpoint, at },
            decode_status_reply,
        )
    }

    /// Play out everything queued on `endpoint`, then stop.
    ///
    /// # Errors
    ///
    /// A transport failure, or the driver's typed refusal.
    pub fn drain(&mut self, endpoint: u16) -> Result<(), Errno> {
        self.exchange(AudioChannelRequest::Drain { endpoint }, decode_status_reply)
    }

    /// The doorbell: move one period between the shared ring and the device
    /// and report the clock pair.
    ///
    /// # Errors
    ///
    /// A transport failure, or the driver's typed refusal (a service before
    /// attach, or a device fault).
    pub fn service(&mut self, endpoint: u16) -> Result<AudioServiceReport, Errno> {
        self.exchange(
            AudioChannelRequest::Service { endpoint },
            decode_service_reply,
        )
    }

    /// Set the endpoint's hardware gain and mute.
    ///
    /// # Errors
    ///
    /// A transport failure, or the driver's typed refusal — including
    /// [`Errno::NotSupported`] from a device with no control of its own, in
    /// which case the mixer keeps the whole gain in software.
    pub fn gain(&mut self, endpoint: u16, millibel: i32, mute: bool) -> Result<(), Errno> {
        self.exchange(
            AudioChannelRequest::Gain {
                endpoint,
                millibel,
                mute,
            },
            decode_status_reply,
        )
    }

    /// Release the channel: the driver unmaps the region and forgets the
    /// notify port.
    ///
    /// # Errors
    ///
    /// A transport failure, or the driver's typed refusal.
    pub fn detach(&mut self, endpoint: u16) -> Result<(), Errno> {
        self.exchange(
            AudioChannelRequest::Detach { endpoint },
            decode_status_reply,
        )
    }

    /// Encode `request`, call the driver, and decode its reply with `decode`.
    ///
    /// The one place a request frame is built and a reply is read, so no
    /// operation can drift from the buffer bounds the contract fixes.
    fn exchange<R>(
        &mut self,
        request: AudioChannelRequest,
        decode: fn(&[u8]) -> Result<R, Errno>,
    ) -> Result<R, Errno> {
        let mut frame = [0u8; AUDIO_CHANNEL_MAX_REQUEST];
        let len = request.encode(&mut frame)?;
        let mut reply = [0u8; AUDIO_CHANNEL_MAX_REPLY];
        let reply_len = self.transport.call(&frame[..len], &mut reply)?;
        decode(&reply[..reply_len])
    }
}
