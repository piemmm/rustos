//! The driver-side per-request handler of the `netchan-v1` device-channel
//! contract (`plans/NETWORK.md` §2.3, N4d).
//!
//! [`NetChannelServer`] is the pure, host-testable engine a link-layer
//! driver *process* drives: the process owns the device (MMIO/DMA/IRQ) and
//! the call endpoint, and hands each decoded
//! [`NetChannelRequest`](tairix_abi::driver::net_channel::NetChannelRequest)
//! to this server, which turns it into the right
//! device action and the matching reply. The I/O — receiving the request,
//! mapping the granted frame region, sending the reply and the
//! receive-frames notify — stays in the crate's `serve` loop; the server
//! holds only the attach state and the ring/service logic, so the whole
//! control plane is exercised on the host against a mock [`Net`].
//!
//! # State machine
//!
//! A freshly-constructed server is **detached**: it answers
//! [`NetChannelRequest::Facts`](tairix_abi::driver::net_channel::NetChannelRequest::Facts)
//! (so the stack can size the ring geometry) and refuses
//! [`NetChannelRequest::Service`](tairix_abi::driver::net_channel::NetChannelRequest::Service)
//! with [`Errno::NotConnected`].
//! [`NetChannelRequest::Attach`](tairix_abi::driver::net_channel::NetChannelRequest::Attach)
//! validates the offered geometry against the
//! device and moves the server to **attached**; from there
//! [`NetChannelServer::service_reply`] binds a [`FrameRings`] view over the
//! caller-mapped region and drives one [`Net::service`] doorbell.
//! [`NetChannelRequest::Detach`](tairix_abi::driver::net_channel::NetChannelRequest::Detach)
//! returns the server to detached.
//!
//! # Fail closed
//!
//! Every reply is a fully-encoded `netchan-v1` frame. A service call before
//! attach, a region whose length does not match the agreed geometry, a
//! geometry too small to carry the device's frames, or any device fault is a
//! typed [`Errno`] carried in the reply's status word — never a panic, never
//! a partially-applied action.

use tairix_abi::driver::net::Net;
use tairix_abi::driver::net_channel::{
    encode_facts_reply, encode_service_reply, AttachParams, NET_CHANNEL_FACTS_REPLY_LEN,
    NET_CHANNEL_SERVICE_REPLY_LEN,
};
use tairix_abi::driver::net_ring::{FrameRings, RingGeometry, ServiceReport};
use tairix_abi::driver::BufferClass;
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::Errno;

/// The state a channel carries once the stack has attached its frame region.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Attached {
    /// The ring geometry both sides agreed (the stack sized it from the
    /// device MTU); every [`FrameRings`] view binds with exactly this.
    geometry: RingGeometry,
    /// Sensitivity class the device honours when scrubbing its staging.
    class: BufferClass,
    /// Numeric IPC endpoint the driver `ipc_send`s a receive-frames notify
    /// to (the stack bound and owns it).
    notify_endpoint: u64,
}

/// The driver-side handler of one NIC device channel.
///
/// Wraps the concrete [`Net`] device engine and tracks whether the stack has
/// attached a frame region. It never performs I/O: the process binary
/// receives the request, maps the region, and sends the reply this server
/// produces.
pub struct NetChannelServer<N: Net> {
    net: N,
    attached: Option<Attached>,
}

impl<N: Net> NetChannelServer<N> {
    /// Build a detached server over the device engine `net`.
    pub fn new(net: N) -> Self {
        Self {
            net,
            attached: None,
        }
    }

    /// Borrow the underlying device engine (host-test access).
    #[must_use]
    pub fn net(&self) -> &N {
        &self.net
    }

    /// Borrow the underlying device engine mutably.
    ///
    /// The driver process needs this to acknowledge the device interrupt
    /// (`Transport::ack_interrupt`) the moment its IRQ fires — before it wakes
    /// the stack with a notify and independently of whether a frame region is
    /// attached — so a receive interrupt is deasserted promptly and never
    /// re-fires in a storm while the stack has not yet issued its next
    /// service doorbell.
    pub fn net_mut(&mut self) -> &mut N {
        &mut self.net
    }

    /// Whether the stack has attached a frame region.
    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.attached.is_some()
    }

    /// The numeric IPC endpoint the driver notifies when receive frames
    /// arrive, or [`None`] while detached.
    #[must_use]
    pub fn notify_endpoint(&self) -> Option<u64> {
        self.attached.as_ref().map(|a| a.notify_endpoint)
    }

    /// The agreed ring geometry, or [`None`] while detached. The process
    /// binary sizes the region it maps from
    /// [`RingGeometry::region_len`](RingGeometry::region_len).
    #[must_use]
    pub fn geometry(&self) -> Option<RingGeometry> {
        self.attached.as_ref().map(|a| a.geometry)
    }

    /// Answer [`NetChannelRequest::Facts`](tairix_abi::driver::net_channel::NetChannelRequest::Facts):
    /// the device's validated facts, or a `-errno` status on a device fault.
    #[must_use]
    pub fn facts_reply(&self) -> [u8; NET_CHANNEL_FACTS_REPLY_LEN] {
        encode_facts_reply(
            self.net
                .device_facts()
                .map_err(tairix_abi::DriverError::as_errno),
        )
    }

    /// Answer [`NetChannelRequest::Attach`](tairix_abi::driver::net_channel::NetChannelRequest::Attach):
    /// validate the offered geometry against the device and, on success,
    /// move to attached. Returns the status frame; on refusal the server is
    /// left unchanged (fail closed — a rejected attach never half-binds).
    ///
    /// The geometry's slot must be able to carry the device's largest frame
    /// (its MTU plus the Ethernet header); a stack that offered a smaller
    /// ring than it promised via the [`facts_reply`](Self::facts_reply) is
    /// refused rather than silently dropping every oversize frame later.
    #[must_use]
    pub fn attach(&mut self, params: AttachParams) -> [u8; STATUS_REPLY_LEN] {
        let result = self.try_attach(params);
        encode_status_reply(result)
    }

    fn try_attach(&mut self, params: AttachParams) -> Result<(), Errno> {
        let facts = self
            .net
            .device_facts()
            .map_err(tairix_abi::DriverError::as_errno)?;
        // Both directions must carry at least one device frame; when the
        // device segments (`TX_SEGMENT_TCP`) the transmit ring must
        // additionally carry a super-frame. `for_device` is the one
        // definition of those minima (the stack sized its offer from the
        // same facts), so a ring smaller than the device needs is refused
        // rather than silently dropping oversize frames later.
        let need = RingGeometry::for_device(&facts, params.geometry.slots())?;
        if params.geometry.rx_slot_capacity() < need.rx_slot_capacity()
            || params.geometry.tx_slot_capacity() < need.tx_slot_capacity()
        {
            return Err(Errno::OutOfRange);
        }
        self.attached = Some(Attached {
            geometry: params.geometry,
            class: params.class,
            notify_endpoint: params.notify_endpoint,
        });
        Ok(())
    }

    /// Answer [`NetChannelRequest::Service`](tairix_abi::driver::net_channel::NetChannelRequest::Service):
    /// bind the frame rings over the caller-mapped `region` and drive one
    /// [`Net::service`] doorbell, reporting what moved.
    ///
    /// `region` is the driver's own mapping of the shared frame region (the
    /// process binary `shm_map`ped the grant the stack forwarded in
    /// [`Attach`](tairix_abi::driver::net_channel::NetChannelRequest::Attach)).
    /// A service before attach fails closed with [`Errno::NotConnected`]; a
    /// region whose length does not match the agreed geometry, or a device
    /// fault, is the typed error carried in the reply.
    #[must_use]
    pub fn service_reply(&mut self, region: &mut [u8]) -> [u8; NET_CHANNEL_SERVICE_REPLY_LEN] {
        encode_service_reply(self.service(region))
    }

    fn service(&mut self, region: &mut [u8]) -> Result<ServiceReport, Errno> {
        let attached = self.attached.as_ref().ok_or(Errno::NotConnected)?;
        let mut rings = FrameRings::bind(region, attached.geometry, attached.class)?;
        self.net
            .service(&mut rings)
            .map_err(tairix_abi::DriverError::as_errno)
    }

    /// Answer [`NetChannelRequest::Detach`](tairix_abi::driver::net_channel::NetChannelRequest::Detach):
    /// forget the attach state. The process binary unmaps the region and
    /// forgets the notify port; the device itself stays live for a later
    /// re-attach.
    #[must_use]
    pub fn detach(&mut self) -> [u8; STATUS_REPLY_LEN] {
        self.attached = None;
        encode_status_reply(Ok(()))
    }
}

#[cfg(test)]
mod tests;
