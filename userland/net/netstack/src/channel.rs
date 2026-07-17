//! The stack-side frame-service abstraction (`plans/NETWORK.md` N4d).
//!
//! [`Netstack::service_interface`](crate::Netstack::service_interface) pumps
//! one interface's frames — engine output into the TX ring, a device
//! doorbell, delivered frames back through the engine — and that pump must be
//! written **once** whether the link-layer driver runs in-process (host
//! tests) or in a separate driver process (the live service). Both shapes
//! implement the [`FrameService`] seam:
//!
//! * [`LocalFrameService`] wraps an in-process [`Net`] device engine over a
//!   plain owned region — the host-test and single-address-space form.
//! * [`NetChannelClient`] is the cross-process client of the `netchan-v1`
//!   contract ([`tairix_abi::driver::net_channel`]): it owns the stack's
//!   mapping of the shared frame region and turns each doorbell into an
//!   `ipc_call` to the driver process's device endpoint, over the injected
//!   [`NetChannelTransport`] so it stays pure and host-testable.
//!
//! The frame region a [`FrameService`] owns is exactly
//! [`RingGeometry::region_len`] bytes; the pump binds a [`FrameRings`] view
//! over it for each phase and never touches ring bytes across the doorbell,
//! so the call boundary is the whole synchronisation (`net_ring`).

use tairix_abi::driver::net::{DeviceFacts, Net};
use tairix_abi::driver::net_channel::{
    decode_facts_reply, decode_service_reply, AttachParams, NetChannelRequest,
    NET_CHANNEL_MAX_REPLY,
};
use tairix_abi::driver::net_ring::{FrameRings, RingGeometry, ServiceReport};
use tairix_abi::driver::BufferClass;
use tairix_abi::reply::decode_status_reply;
use tairix_abi::Errno;

/// A link-layer frame service the interface pump drives.
///
/// The implementor owns the shared frame region and knows the agreed
/// geometry and sensitivity class; the pump borrows the region to queue and
/// harvest frames and calls [`service`](FrameService::service) as the
/// doorbell between phases. One seam, two implementors ([`LocalFrameService`]
/// and [`NetChannelClient`]), so the pump is defined once.
pub trait FrameService {
    /// The agreed ring geometry (both directions share it).
    fn geometry(&self) -> RingGeometry;

    /// The traffic sensitivity class the rings carry.
    fn class(&self) -> BufferClass;

    /// Borrow the shared frame region (exactly [`RingGeometry::region_len`]
    /// bytes) to bind a [`FrameRings`] view over.
    fn region_mut(&mut self) -> &mut [u8];

    /// The doorbell: service the rings once and report what moved.
    ///
    /// # Errors
    ///
    /// A device fault or a corrupt ring state as a typed [`Errno`].
    fn service(&mut self) -> Result<ServiceReport, Errno>;
}

/// An in-process [`Net`] device engine presented as a [`FrameService`] over
/// a region the wrapper owns.
///
/// The single-address-space form: the same physical bytes back both the
/// pump's ring view and the device's service, so `service` binds a
/// [`FrameRings`] over the owned region and drives [`Net::service`] directly.
pub struct LocalFrameService<'r, N: Net> {
    net: N,
    region: &'r mut [u8],
    geometry: RingGeometry,
    class: BufferClass,
}

impl<'r, N: Net> LocalFrameService<'r, N> {
    /// Wrap `net` over `region` with the agreed `geometry` and `class`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `region` is not exactly
    /// [`RingGeometry::region_len`] bytes.
    pub fn new(
        net: N,
        region: &'r mut [u8],
        geometry: RingGeometry,
        class: BufferClass,
    ) -> Result<Self, Errno> {
        if region.len() != geometry.region_len() {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self {
            net,
            region,
            geometry,
            class,
        })
    }

    /// Borrow the wrapped device engine.
    #[must_use]
    pub fn net(&self) -> &N {
        &self.net
    }

    /// Borrow the wrapped device engine mutably (host-test drivers that
    /// advance their own clock between pumps).
    pub fn net_mut(&mut self) -> &mut N {
        &mut self.net
    }
}

impl<N: Net> FrameService for LocalFrameService<'_, N> {
    fn geometry(&self) -> RingGeometry {
        self.geometry
    }

    fn class(&self) -> BufferClass {
        self.class
    }

    fn region_mut(&mut self) -> &mut [u8] {
        self.region
    }

    fn service(&mut self) -> Result<ServiceReport, Errno> {
        let mut rings = FrameRings::bind(self.region, self.geometry, self.class)?;
        self.net
            .service(&mut rings)
            .map_err(tairix_abi::DriverError::as_errno)
    }
}

/// The injected doorbell transport of a [`NetChannelClient`].
///
/// One `ipc_call` to the driver process's device endpoint: the live service
/// backs it with `tairix_rt::ipc_call` (an optional bare-metal-only
/// dependency), and host tests back it with an in-process fake that
/// dispatches to a `tairix_virtio_net::NetChannelServer` (a dev-dependency),
/// so the client is exercised without a kernel.
pub trait NetChannelTransport {
    /// Send `request` to the driver endpoint and copy its reply into
    /// `reply`, returning the reply length.
    ///
    /// # Errors
    ///
    /// The transport's typed [`Errno`] — a destroyed endpoint, an oversize
    /// message, or a reply larger than `reply`.
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno>;
}

/// The stack-side client of a NIC driver process's `netchan-v1` endpoint.
///
/// Owns the stack's mapping of the shared frame region (the driver holds its
/// own mapping of the same physical frames) and issues each doorbell as a
/// [`NetChannelRequest::Service`] over the [`NetChannelTransport`]. Presented
/// as a [`FrameService`] so the one interface pump drives it.
pub struct NetChannelClient<'r, T: NetChannelTransport> {
    transport: T,
    region: &'r mut [u8],
    geometry: RingGeometry,
    class: BufferClass,
}

impl<T: NetChannelTransport> NetChannelClient<'_, T> {
    /// Query the driver's [`DeviceFacts`] before a region exists, so the
    /// stack can size the ring geometry.
    ///
    /// # Errors
    ///
    /// A transport error, or the driver's typed refusal.
    pub fn query_facts(transport: &mut T) -> Result<DeviceFacts, Errno> {
        let mut request = [0u8; NetChannelRequest::MAX_WIRE_LEN];
        let len = NetChannelRequest::Facts.encode(&mut request)?;
        let mut reply = [0u8; NET_CHANNEL_MAX_REPLY];
        let reply_len = transport.call(&request[..len], &mut reply)?;
        decode_facts_reply(&reply[..reply_len])
    }
}

impl<'r, T: NetChannelTransport> NetChannelClient<'r, T> {
    /// Attach the mapped `region` to the driver and start frame flow.
    ///
    /// Sends [`NetChannelRequest::Attach`] with the agreed `geometry`, the
    /// `region_grant` handle the stack minted for the driver endpoint, the
    /// traffic `class`, and the `notify_endpoint` the driver wakes on. On the
    /// driver's success reply the client owns the channel.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `region` is not exactly
    ///   [`RingGeometry::region_len`] bytes.
    /// * A transport error, or the driver's typed refusal (the channel is
    ///   not established and the caller unmaps the region).
    #[allow(clippy::too_many_arguments)]
    pub fn attach(
        mut transport: T,
        region: &'r mut [u8],
        geometry: RingGeometry,
        class: BufferClass,
        region_grant: u64,
        notify_endpoint: u64,
    ) -> Result<Self, Errno> {
        if region.len() != geometry.region_len() {
            return Err(Errno::BufferTooSmall);
        }
        let params = AttachParams {
            geometry,
            region_grant,
            class,
            notify_endpoint,
        };
        let mut request = [0u8; NetChannelRequest::MAX_WIRE_LEN];
        let len = NetChannelRequest::Attach(params).encode(&mut request)?;
        let mut reply = [0u8; NET_CHANNEL_MAX_REPLY];
        let reply_len = transport.call(&request[..len], &mut reply)?;
        decode_status_reply(&reply[..reply_len])?;
        Ok(Self {
            transport,
            region,
            geometry,
            class,
        })
    }

    /// Release the channel: ask the driver to detach and forget the region.
    ///
    /// # Errors
    ///
    /// A transport error, or the driver's typed refusal.
    pub fn detach(mut self) -> Result<(), Errno> {
        let mut request = [0u8; NetChannelRequest::MAX_WIRE_LEN];
        let len = NetChannelRequest::Detach.encode(&mut request)?;
        let mut reply = [0u8; NET_CHANNEL_MAX_REPLY];
        let reply_len = self.transport.call(&request[..len], &mut reply)?;
        decode_status_reply(&reply[..reply_len])
    }

    fn doorbell(&mut self) -> Result<ServiceReport, Errno> {
        let mut request = [0u8; NetChannelRequest::MAX_WIRE_LEN];
        let len = NetChannelRequest::Service.encode(&mut request)?;
        let mut reply = [0u8; NET_CHANNEL_MAX_REPLY];
        let reply_len = self.transport.call(&request[..len], &mut reply)?;
        decode_service_reply(&reply[..reply_len])
    }
}

impl<T: NetChannelTransport> FrameService for NetChannelClient<'_, T> {
    fn geometry(&self) -> RingGeometry {
        self.geometry
    }

    fn class(&self) -> BufferClass {
        self.class
    }

    fn region_mut(&mut self) -> &mut [u8] {
        self.region
    }

    fn service(&mut self) -> Result<ServiceReport, Errno> {
        self.doorbell()
    }
}
