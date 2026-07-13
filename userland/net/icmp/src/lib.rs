//! Userland ARP / IPv4 / ICMP-echo responder (`userland/net/icmp`).
//!
//! This crate is the smallest network service RustOS ships. It has two
//! halves: [`Responder`] answers ARP requests for a single configured
//! IPv4 address and replies to ICMP echo requests ("ping") aimed at
//! that address, and [`Client`] is the initiating counterpart that
//! resolves a peer's link-layer address via ARP and pings it. It is
//! the protocol peer the virtio-net QEMU integration tests exercise
//! (`PLAN.md` Stage 4.D): the test bin uses [`Client`] to resolve and
//! ping the QEMU user-network gateway over the live virtio-net device.
//!
//! # Scope
//!
//! In scope: ARP request + reply (RFC 826), IPv4 (RFC 791, option-free
//! headers only), and ICMP echo (RFC 792), all parsed and emitted by
//! the shared protocol engine (`lib/net`), which this crate re-exports.
//! Explicitly **out of scope**: TCP, UDP, IPv6, IP routing,
//! fragmentation, and any form of neighbour cache or retransmission —
//! the `netstack` service (`plans/NETWORK.md`) delivers those and
//! replaces this responder outright in its N3 increment.
//!
//! # Design
//!
//! The crate is `no_std` and allocation-free. [`Responder`] holds the
//! interface's link-layer and IPv4 addresses and is otherwise
//! stateless: [`Responder::handle_frame`] is a pure function from an
//! inbound frame plus a caller-owned scratch buffer to an optional
//! outbound frame. [`Responder::poll`] and [`Responder::run`] drive
//! that logic over any [`Net`] driver through a caller-owned
//! [`FrameRings`] pair (the shared-memory frame-ring transport), so
//! the same code runs against a real virtio-net device and against a
//! mock in unit tests.
//!
//! # Security
//!
//! The responder performs no privileged operation itself; it only
//! transforms bytes. Capability enforcement for [`Net::service`]
//! (`CAP_NET_RAW`) happens at the driver dispatch site, upstream of
//! this crate. Frames addressed
//! to neither this host nor the broadcast address are ignored, and a
//! reply is only emitted for a request that is well-formed, correctly
//! addressed, and (for ICMP) checksum-valid; everything else is
//! dropped silently rather than answered.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub use rustos_net::addr::Ipv4Addr;
pub use rustos_net::{arp, eth as ethernet, icmp, ipv4};

#[cfg(test)]
use rustos_abi::driver::net::{DeviceFacts, LinkState, NetOffloads};
use rustos_abi::driver::net::{MacAddress, Net};
use rustos_abi::driver::net_ring::FrameRings;
use rustos_abi::{DriverError, Errno};

use crate::arp::ArpPacket;
use crate::ethernet::EthernetFrame;
use crate::icmp::IcmpEcho;
use crate::ipv4::Ipv4Header;

/// Error returned while servicing a frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NetServiceError {
    /// The underlying [`Net`] driver returned an error.
    Driver(DriverError),
    /// A frame-ring operation failed (corrupt state or no space).
    Ring(Errno),
    /// The supplied transmit buffer was too small to hold the reply.
    OutputTooSmall,
}

impl From<DriverError> for NetServiceError {
    fn from(error: DriverError) -> Self {
        Self::Driver(error)
    }
}

impl From<Errno> for NetServiceError {
    fn from(error: Errno) -> Self {
        Self::Ring(error)
    }
}

/// Stateless ARP + ICMP-echo responder for one network interface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Responder {
    mac: MacAddress,
    ip: Ipv4Addr,
}

impl Responder {
    /// Bind the responder to an interface's addresses.
    #[must_use]
    pub const fn new(mac: MacAddress, ip: Ipv4Addr) -> Self {
        Self { mac, ip }
    }

    /// The link-layer address this responder answers for.
    #[must_use]
    pub const fn mac_address(&self) -> MacAddress {
        self.mac
    }

    /// The IPv4 address this responder answers for.
    #[must_use]
    pub const fn ipv4_address(&self) -> Ipv4Addr {
        self.ip
    }

    /// Process one inbound `frame`, writing any reply into `out`.
    ///
    /// Returns `Ok(Some(len))` with the reply length when a reply was
    /// produced, `Ok(None)` when the frame needs no answer (not for us,
    /// malformed, or an unsupported protocol), and
    /// [`NetServiceError::OutputTooSmall`] when `out` cannot hold the
    /// reply.
    pub fn handle_frame(
        &self,
        frame: &[u8],
        out: &mut [u8],
    ) -> Result<Option<usize>, NetServiceError> {
        let Some(eth) = EthernetFrame::parse(frame) else {
            return Ok(None);
        };
        if !eth.addressed_to(self.mac) {
            return Ok(None);
        }
        match eth.ethertype {
            ethernet::ETHERTYPE_ARP => self.answer_arp(eth.payload, out),
            ethernet::ETHERTYPE_IPV4 => self.answer_ipv4(eth.payload, eth.source, out),
            _ => Ok(None),
        }
    }

    /// Run a single service/answer cycle over `net`.
    ///
    /// Services the rings once (delivering pending frames), answers
    /// the oldest received frame, and queues + flushes any reply.
    /// Returns `Ok(true)` when a frame was received and processed (it
    /// may or may not have warranted a reply) and `Ok(false)` when no
    /// frame was pending. `rx` is the scratch buffer frames are popped
    /// into and `tx` the buffer replies are assembled in.
    pub fn poll<N: Net>(
        &self,
        net: &mut N,
        rings: &mut FrameRings<'_>,
        rx: &mut [u8],
        tx: &mut [u8],
    ) -> Result<bool, NetServiceError> {
        net.service(rings)?;
        let Some(received) = rings.rx.pop(rx)? else {
            return Ok(false);
        };
        if let Some(len) = self.handle_frame(&rx[..received], tx)? {
            rings.tx.push(&tx[..len])?;
            net.service(rings)?;
        }
        Ok(true)
    }

    /// Poll `net` up to `max_polls` times, returning how many frames
    /// were received and processed.
    ///
    /// The bound keeps the loop finite for tests and for callers that
    /// interleave other work; each poll's device wait is event-driven
    /// inside [`Net::service`], so the budget bounds churn, not a
    /// spin.
    pub fn run<N: Net>(
        &self,
        net: &mut N,
        rings: &mut FrameRings<'_>,
        rx: &mut [u8],
        tx: &mut [u8],
        max_polls: usize,
    ) -> Result<usize, NetServiceError> {
        let mut handled = 0;
        for _ in 0..max_polls {
            if self.poll(net, rings, rx, tx)? {
                handled += 1;
            }
        }
        Ok(handled)
    }

    fn answer_arp(&self, payload: &[u8], out: &mut [u8]) -> Result<Option<usize>, NetServiceError> {
        let Some(request) = ArpPacket::parse(payload) else {
            return Ok(None);
        };
        if request.operation != arp::OP_REQUEST || request.target_protocol != self.ip {
            return Ok(None);
        }
        let reply = request.reply_from(self.mac);
        let len = write_arp_frame(out, request.sender_hardware, self.mac, &reply)?;
        Ok(Some(len))
    }

    fn answer_ipv4(
        &self,
        payload: &[u8],
        peer_mac: MacAddress,
        out: &mut [u8],
    ) -> Result<Option<usize>, NetServiceError> {
        let Some((header, _options, datagram)) = Ipv4Header::parse(payload) else {
            return Ok(None);
        };
        if header.protocol != ipv4::PROTOCOL_ICMP || header.destination != self.ip {
            return Ok(None);
        }
        let Some(request) = IcmpEcho::parse(icmp::IcmpContext::V4, datagram) else {
            return Ok(None);
        };
        if request.kind != icmp::EchoKind::Request {
            return Ok(None);
        }
        let echo = request.reply();
        let reply_header = Ipv4Header::new(self.ip, header.source, ipv4::PROTOCOL_ICMP);
        let len = write_icmp_frame(out, peer_mac, self.mac, &reply_header, &echo)?;
        Ok(Some(len))
    }
}

/// Active ARP + ICMP-echo client for one network interface.
///
/// [`Responder`] answers inbound requests; `Client` is its initiating
/// counterpart. It builds an ARP request to resolve a peer's
/// link-layer address and an ICMP echo request ("ping") to that peer,
/// and recognises the matching replies. [`Client::resolve`] and
/// [`Client::ping`] drive that logic over any [`Net`] driver, so the
/// same code resolves and pings the QEMU user-network gateway in the
/// virtio-net integration tests (`PLAN.md` Stage 4.D) and runs against
/// a mock in unit tests.
///
/// Like [`Responder`] the type is stateless: it neither caches
/// resolved addresses nor retransmits. The `netstack` service layers
/// the shared neighbour cache (`rustos_net::neigh`) and retries on
/// top (`plans/NETWORK.md`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Client {
    mac: MacAddress,
    ip: Ipv4Addr,
}

impl Client {
    /// Bind the client to an interface's addresses.
    #[must_use]
    pub const fn new(mac: MacAddress, ip: Ipv4Addr) -> Self {
        Self { mac, ip }
    }

    /// The link-layer address this client sources frames from.
    #[must_use]
    pub const fn mac_address(&self) -> MacAddress {
        self.mac
    }

    /// The IPv4 address this client sources datagrams from.
    #[must_use]
    pub const fn ipv4_address(&self) -> Ipv4Addr {
        self.ip
    }

    /// Serialise a broadcast ARP request resolving `target` into `out`.
    ///
    /// Returns the frame length, or [`NetServiceError::OutputTooSmall`]
    /// when `out` cannot hold it.
    pub fn write_arp_request(
        &self,
        target: Ipv4Addr,
        out: &mut [u8],
    ) -> Result<usize, NetServiceError> {
        let request = ArpPacket {
            operation: arp::OP_REQUEST,
            sender_hardware: self.mac,
            sender_protocol: self.ip,
            target_hardware: MacAddress([0; 6]),
            target_protocol: target,
        };
        write_arp_frame(out, ethernet::BROADCAST, self.mac, &request)
    }

    /// Interpret `frame` as the ARP reply that binds `target`,
    /// returning the resolved link-layer address.
    ///
    /// Returns `None` when the frame is not an ARP reply addressed to
    /// this client for `target`.
    #[must_use]
    pub fn parse_arp_reply(&self, frame: &[u8], target: Ipv4Addr) -> Option<MacAddress> {
        let eth = EthernetFrame::parse(frame)?;
        if !eth.addressed_to(self.mac) || eth.ethertype != ethernet::ETHERTYPE_ARP {
            return None;
        }
        let arp = ArpPacket::parse(eth.payload)?;
        if arp.operation != arp::OP_REPLY || arp.sender_protocol != target {
            return None;
        }
        Some(arp.sender_hardware)
    }

    /// Serialise an ICMP echo request to `(peer_mac, dest)` into `out`.
    ///
    /// `peer_mac` is the link-layer destination (typically resolved via
    /// [`Self::resolve`]); `dest` is the IPv4 destination. Returns the
    /// frame length, or [`NetServiceError::OutputTooSmall`].
    pub fn write_echo_request(
        &self,
        peer_mac: MacAddress,
        dest: Ipv4Addr,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
        out: &mut [u8],
    ) -> Result<usize, NetServiceError> {
        let echo = IcmpEcho {
            kind: icmp::EchoKind::Request,
            identifier,
            sequence,
            payload,
        };
        let header = Ipv4Header::new(self.ip, dest, ipv4::PROTOCOL_ICMP);
        write_icmp_frame(out, peer_mac, self.mac, &header, &echo)
    }

    /// Interpret `frame` as the ICMP echo reply matching a request this
    /// client sent to `dest` with `identifier`/`sequence`.
    ///
    /// Returns `true` only for a well-formed, checksum-valid echo reply
    /// addressed to this client, sourced from `dest`, and carrying the
    /// expected identifier and sequence.
    #[must_use]
    pub fn is_echo_reply(
        &self,
        frame: &[u8],
        dest: Ipv4Addr,
        identifier: u16,
        sequence: u16,
    ) -> bool {
        let Some(eth) = EthernetFrame::parse(frame) else {
            return false;
        };
        if !eth.addressed_to(self.mac) || eth.ethertype != ethernet::ETHERTYPE_IPV4 {
            return false;
        }
        let Some((header, _options, datagram)) = Ipv4Header::parse(eth.payload) else {
            return false;
        };
        if header.protocol != ipv4::PROTOCOL_ICMP
            || header.source != dest
            || header.destination != self.ip
        {
            return false;
        }
        let Some(echo) = IcmpEcho::parse(icmp::IcmpContext::V4, datagram) else {
            return false;
        };
        echo.kind == icmp::EchoKind::Reply
            && echo.identifier == identifier
            && echo.sequence == sequence
    }

    /// Resolve `target`'s link-layer address over `net`.
    ///
    /// Queues one ARP request, then services the rings up to
    /// `max_polls` times looking for the matching reply. Returns
    /// `Ok(Some(mac))` once resolved, `Ok(None)` if no reply arrived
    /// within the budget, and an error if the driver or a buffer
    /// fails. `rx` is the scratch buffer frames are popped into and
    /// `tx` the buffer the request is assembled in.
    pub fn resolve<N: Net>(
        &self,
        net: &mut N,
        rings: &mut FrameRings<'_>,
        target: Ipv4Addr,
        rx: &mut [u8],
        tx: &mut [u8],
        max_polls: usize,
    ) -> Result<Option<MacAddress>, NetServiceError> {
        let len = self.write_arp_request(target, tx)?;
        rings.tx.push(&tx[..len])?;
        for _ in 0..max_polls {
            net.service(rings)?;
            let Some(received) = rings.rx.pop(rx)? else {
                continue;
            };
            if let Some(mac) = self.parse_arp_reply(&rx[..received], target) {
                return Ok(Some(mac));
            }
        }
        Ok(None)
    }

    /// Ping `dest` over `net` and confirm the echo reply.
    ///
    /// Queues one ICMP echo request to the already-resolved
    /// `peer_mac` / `dest`, then services the rings up to `max_polls`
    /// times looking for the matching echo reply. Returns `Ok(true)`
    /// once the reply arrives, `Ok(false)` if none did within the
    /// budget.
    #[allow(clippy::too_many_arguments)]
    pub fn ping<N: Net>(
        &self,
        net: &mut N,
        rings: &mut FrameRings<'_>,
        peer_mac: MacAddress,
        dest: Ipv4Addr,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
        rx: &mut [u8],
        tx: &mut [u8],
        max_polls: usize,
    ) -> Result<bool, NetServiceError> {
        let len = self.write_echo_request(peer_mac, dest, identifier, sequence, payload, tx)?;
        rings.tx.push(&tx[..len])?;
        for _ in 0..max_polls {
            net.service(rings)?;
            let Some(received) = rings.rx.pop(rx)? else {
                continue;
            };
            if self.is_echo_reply(&rx[..received], dest, identifier, sequence) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Write an Ethernet II + ARP frame into `out`: a 14-byte header
/// (`ETHERTYPE_ARP`, `dst`/`src`) followed by `arp`.
///
/// Returns the total length, or [`NetServiceError::OutputTooSmall`]
/// when `out` cannot hold the header and packet. Shared by
/// [`Responder`] (reply) and [`Client`] (request) so the framing is
/// written once.
fn write_arp_frame(
    out: &mut [u8],
    dst: MacAddress,
    src: MacAddress,
    arp: &ArpPacket,
) -> Result<usize, NetServiceError> {
    let eth_len = ethernet::write_header(out, dst, src, ethernet::ETHERTYPE_ARP)
        .ok_or(NetServiceError::OutputTooSmall)?;
    let body = out
        .get_mut(eth_len..)
        .ok_or(NetServiceError::OutputTooSmall)?;
    let arp_len = arp.write(body).ok_or(NetServiceError::OutputTooSmall)?;
    Ok(eth_len + arp_len)
}

/// Write an Ethernet II + IPv4 + ICMP echo frame into `out`.
///
/// Returns the total length, or [`NetServiceError::OutputTooSmall`].
/// Shared by [`Responder`] (echo reply) and [`Client`] (echo request).
fn write_icmp_frame(
    out: &mut [u8],
    dst: MacAddress,
    src: MacAddress,
    ip_header: &Ipv4Header,
    echo: &IcmpEcho<'_>,
) -> Result<usize, NetServiceError> {
    let eth_len = ethernet::write_header(out, dst, src, ethernet::ETHERTYPE_IPV4)
        .ok_or(NetServiceError::OutputTooSmall)?;
    let icmp_len = echo.wire_len();
    let icmp_start = eth_len + ipv4::IPV4_HEADER_LEN;
    let icmp_buf = out
        .get_mut(icmp_start..)
        .ok_or(NetServiceError::OutputTooSmall)?;
    echo.write(icmp::IcmpContext::V4, icmp_buf)
        .ok_or(NetServiceError::OutputTooSmall)?;
    let ip_buf = out
        .get_mut(eth_len..)
        .ok_or(NetServiceError::OutputTooSmall)?;
    ip_header
        .write(ip_buf, icmp_len)
        .ok_or(NetServiceError::OutputTooSmall)?;
    Ok(eth_len + ipv4::IPV4_HEADER_LEN + icmp_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arp::ArpPacket;
    use crate::icmp::IcmpEcho;

    const LOCAL_MAC: MacAddress = MacAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    const LOCAL_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
    const PEER_MAC: MacAddress = MacAddress([0x02, 0xCA, 0xFE, 0xBA, 0xBE, 0x01]);
    const PEER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

    fn responder() -> Responder {
        Responder::new(LOCAL_MAC, LOCAL_IP)
    }

    use rustos_abi::driver::net_ring::{RingGeometry, ServiceReport};
    use rustos_abi::driver::BufferClass;

    /// Test ring geometry: four slots of 256 bytes per ring.
    const GEOMETRY: RingGeometry = match RingGeometry::new(4, 256) {
        Ok(g) => g,
        Err(_) => panic!("valid test geometry"),
    };
    const REGION_LEN: usize = GEOMETRY.region_len();

    fn bind_rings(region: &mut [u8; REGION_LEN]) -> FrameRings<'_> {
        FrameRings::bind(region, GEOMETRY, BufferClass::NonSensitive).expect("bind rings")
    }

    /// A [`Net`] mock holding at most one queued inbound frame and
    /// capturing the most recent transmitted frame.
    struct MockNet {
        rx: [u8; 256],
        rx_len: usize,
        delivered: bool,
        tx: [u8; 256],
        tx_len: usize,
    }

    impl MockNet {
        fn with_inbound(frame: &[u8]) -> Self {
            let mut rx = [0u8; 256];
            rx[..frame.len()].copy_from_slice(frame);
            Self {
                rx,
                rx_len: frame.len(),
                delivered: false,
                tx: [0u8; 256],
                tx_len: 0,
            }
        }

        fn empty() -> Self {
            Self {
                rx: [0u8; 256],
                rx_len: 0,
                delivered: true,
                tx: [0u8; 256],
                tx_len: 0,
            }
        }

        fn transmitted(&self) -> &[u8] {
            &self.tx[..self.tx_len]
        }
    }

    fn local_facts() -> DeviceFacts {
        DeviceFacts {
            mac: LOCAL_MAC,
            mtu: 1500,
            link: LinkState::Up,
            offloads: NetOffloads::empty(),
            rx_queues: 1,
        }
    }

    impl Net for MockNet {
        fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
            Ok(local_facts())
        }

        fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
            let mut report = ServiceReport::default();
            // Drain queued transmits into the capture buffer.
            let mut scratch = [0u8; 256];
            while let Some(len) = rings
                .tx
                .pop(&mut scratch)
                .map_err(|_| DriverError::BadMagic)?
            {
                self.tx[..len].copy_from_slice(&scratch[..len]);
                self.tx_len = len;
                report.transmitted += 1;
            }
            // Deliver the single queued inbound frame once.
            if !self.delivered && self.rx_len > 0 {
                rings
                    .rx
                    .push(&self.rx[..self.rx_len])
                    .map_err(|_| DriverError::BadMagic)?;
                self.delivered = true;
                report.received += 1;
            }
            Ok(report)
        }
    }

    fn arp_request_frame() -> [u8; ethernet::ETHERNET_HEADER_LEN + arp::ARP_PACKET_LEN] {
        let mut frame = [0u8; ethernet::ETHERNET_HEADER_LEN + arp::ARP_PACKET_LEN];
        ethernet::write_header(
            &mut frame,
            ethernet::BROADCAST,
            PEER_MAC,
            ethernet::ETHERTYPE_ARP,
        )
        .expect("eth header fits");
        let request = ArpPacket {
            operation: arp::OP_REQUEST,
            sender_hardware: PEER_MAC,
            sender_protocol: PEER_IP,
            target_hardware: MacAddress([0; 6]),
            target_protocol: LOCAL_IP,
        };
        request
            .write(&mut frame[ethernet::ETHERNET_HEADER_LEN..])
            .expect("arp body fits");
        frame
    }

    fn icmp_echo_request_frame(payload: &[u8], out: &mut [u8]) -> usize {
        let echo = IcmpEcho {
            kind: icmp::EchoKind::Request,
            identifier: 0xBEEF,
            sequence: 7,
            payload,
        };
        let eth = ethernet::write_header(out, LOCAL_MAC, PEER_MAC, ethernet::ETHERTYPE_IPV4)
            .expect("eth header fits");
        let icmp_len = echo.wire_len();
        echo.write(
            icmp::IcmpContext::V4,
            &mut out[eth + ipv4::IPV4_HEADER_LEN..],
        )
        .expect("icmp fits");
        Ipv4Header::new(PEER_IP, LOCAL_IP, ipv4::PROTOCOL_ICMP)
            .write(&mut out[eth..], icmp_len)
            .expect("ip header fits");
        eth + ipv4::IPV4_HEADER_LEN + icmp_len
    }

    #[test]
    fn answers_arp_request_for_local_ip() {
        let frame = arp_request_frame();
        let mut out = [0u8; 64];
        let len = responder()
            .handle_frame(&frame, &mut out)
            .expect("no error")
            .expect("reply produced");
        let eth = EthernetFrame::parse(&out[..len]).expect("reply parses");
        assert_eq!(eth.destination, PEER_MAC);
        assert_eq!(eth.source, LOCAL_MAC);
        assert_eq!(eth.ethertype, ethernet::ETHERTYPE_ARP);
        let arp = ArpPacket::parse(eth.payload).expect("arp parses");
        assert_eq!(arp.operation, arp::OP_REPLY);
        assert_eq!(arp.sender_hardware, LOCAL_MAC);
        assert_eq!(arp.sender_protocol, LOCAL_IP);
        assert_eq!(arp.target_protocol, PEER_IP);
    }

    #[test]
    fn ignores_arp_for_other_ip() {
        let mut frame = arp_request_frame();
        // Point the ARP target at a different IP.
        let tpa = ethernet::ETHERNET_HEADER_LEN + arp::ARP_PACKET_LEN - 4;
        frame[tpa..].copy_from_slice(&[10, 0, 2, 99]);
        let mut out = [0u8; 64];
        assert_eq!(responder().handle_frame(&frame, &mut out), Ok(None));
    }

    #[test]
    fn answers_icmp_echo_for_local_ip() {
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut frame = [0u8; 128];
        let frame_len = icmp_echo_request_frame(&payload, &mut frame);
        let mut out = [0u8; 128];
        let len = responder()
            .handle_frame(&frame[..frame_len], &mut out)
            .expect("no error")
            .expect("reply produced");
        let eth = EthernetFrame::parse(&out[..len]).expect("reply parses");
        assert_eq!(eth.destination, PEER_MAC);
        assert_eq!(eth.source, LOCAL_MAC);
        let (ip, _options, datagram) = Ipv4Header::parse(eth.payload).expect("ip parses");
        assert_eq!(ip.source, LOCAL_IP);
        assert_eq!(ip.destination, PEER_IP);
        let echo = IcmpEcho::parse(icmp::IcmpContext::V4, datagram).expect("icmp parses");
        assert_eq!(echo.kind, icmp::EchoKind::Reply);
        assert_eq!(echo.identifier, 0xBEEF);
        assert_eq!(echo.sequence, 7);
        assert_eq!(echo.payload, &payload);
    }

    #[test]
    fn ignores_frame_for_other_mac() {
        let mut frame = arp_request_frame();
        // Unicast to a MAC that is neither ours nor broadcast.
        frame[..6].copy_from_slice(&[0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA]);
        let mut out = [0u8; 64];
        assert_eq!(responder().handle_frame(&frame, &mut out), Ok(None));
    }

    #[test]
    fn handle_frame_reports_output_too_small() {
        let frame = arp_request_frame();
        let mut out = [0u8; 8];
        assert_eq!(
            responder().handle_frame(&frame, &mut out),
            Err(NetServiceError::OutputTooSmall)
        );
    }

    #[test]
    fn poll_transmits_a_reply() {
        let frame = arp_request_frame();
        let mut net = MockNet::with_inbound(&frame);
        let mut region = [0u8; REGION_LEN];
        let mut rings = bind_rings(&mut region);
        let mut rx = [0u8; 256];
        let mut tx = [0u8; 256];
        assert_eq!(
            responder().poll(&mut net, &mut rings, &mut rx, &mut tx),
            Ok(true)
        );
        let eth = EthernetFrame::parse(net.transmitted()).expect("reply parses");
        assert_eq!(eth.ethertype, ethernet::ETHERTYPE_ARP);
        // A second poll finds nothing pending.
        assert_eq!(
            responder().poll(&mut net, &mut rings, &mut rx, &mut tx),
            Ok(false)
        );
    }

    #[test]
    fn poll_on_empty_driver_is_idle() {
        let mut net = MockNet::empty();
        let mut region = [0u8; REGION_LEN];
        let mut rings = bind_rings(&mut region);
        let mut rx = [0u8; 64];
        let mut tx = [0u8; 64];
        assert_eq!(
            responder().poll(&mut net, &mut rings, &mut rx, &mut tx),
            Ok(false)
        );
        assert!(net.transmitted().is_empty());
    }

    #[test]
    fn run_counts_processed_frames() {
        let frame = arp_request_frame();
        let mut net = MockNet::with_inbound(&frame);
        let mut region = [0u8; REGION_LEN];
        let mut rings = bind_rings(&mut region);
        let mut rx = [0u8; 256];
        let mut tx = [0u8; 256];
        assert_eq!(
            responder().run(&mut net, &mut rings, &mut rx, &mut tx, 4),
            Ok(1)
        );
    }

    #[test]
    fn poll_propagates_driver_error() {
        // A driver whose service always faults surfaces as Driver(_).
        struct FaultyNet;
        impl Net for FaultyNet {
            fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
                Ok(local_facts())
            }
            fn service(
                &mut self,
                _rings: &mut FrameRings<'_>,
            ) -> Result<ServiceReport, DriverError> {
                Err(DriverError::DeviceFault)
            }
        }
        let mut net = FaultyNet;
        let mut region = [0u8; REGION_LEN];
        let mut rings = bind_rings(&mut region);
        let mut rx = [0u8; 64];
        let mut tx = [0u8; 64];
        assert_eq!(
            responder().poll(&mut net, &mut rings, &mut rx, &mut tx),
            Err(NetServiceError::Driver(DriverError::DeviceFault))
        );
    }

    #[test]
    fn addresses_round_trip() {
        let r = responder();
        assert_eq!(r.mac_address(), LOCAL_MAC);
        assert_eq!(r.ipv4_address(), LOCAL_IP);
    }

    // --- Client (initiator) ------------------------------------------

    fn client() -> Client {
        Client::new(LOCAL_MAC, LOCAL_IP)
    }

    /// The ARP reply the peer would send answering our request.
    fn arp_reply_frame(out: &mut [u8]) -> usize {
        let reply = ArpPacket {
            operation: arp::OP_REPLY,
            sender_hardware: PEER_MAC,
            sender_protocol: PEER_IP,
            target_hardware: LOCAL_MAC,
            target_protocol: LOCAL_IP,
        };
        write_arp_frame(out, LOCAL_MAC, PEER_MAC, &reply).expect("arp reply fits")
    }

    /// The ICMP echo reply the peer would send answering our ping.
    fn echo_reply_frame(identifier: u16, sequence: u16, payload: &[u8], out: &mut [u8]) -> usize {
        let echo = IcmpEcho {
            kind: icmp::EchoKind::Reply,
            identifier,
            sequence,
            payload,
        };
        let header = Ipv4Header::new(PEER_IP, LOCAL_IP, ipv4::PROTOCOL_ICMP);
        write_icmp_frame(out, LOCAL_MAC, PEER_MAC, &header, &echo).expect("echo reply fits")
    }

    #[test]
    fn client_resolve_returns_peer_mac_and_emits_a_request() {
        let mut reply = [0u8; 64];
        let len = arp_reply_frame(&mut reply);
        let mut net = MockNet::with_inbound(&reply[..len]);
        let mut region = [0u8; REGION_LEN];
        let mut rings = bind_rings(&mut region);
        let mut rx = [0u8; 256];
        let mut tx = [0u8; 256];
        let resolved = client()
            .resolve(&mut net, &mut rings, PEER_IP, &mut rx, &mut tx, 4)
            .expect("no error");
        assert_eq!(resolved, Some(PEER_MAC));

        // The transmitted frame is a broadcast ARP request for PEER_IP.
        let eth = EthernetFrame::parse(net.transmitted()).expect("request parses");
        assert_eq!(eth.destination, ethernet::BROADCAST);
        assert_eq!(eth.source, LOCAL_MAC);
        assert_eq!(eth.ethertype, ethernet::ETHERTYPE_ARP);
        let arp = ArpPacket::parse(eth.payload).expect("arp parses");
        assert_eq!(arp.operation, arp::OP_REQUEST);
        assert_eq!(arp.target_protocol, PEER_IP);
    }

    #[test]
    fn client_resolve_is_none_without_a_reply() {
        let mut net = MockNet::empty();
        let mut region = [0u8; REGION_LEN];
        let mut rings = bind_rings(&mut region);
        let mut rx = [0u8; 64];
        let mut tx = [0u8; 64];
        assert_eq!(
            client().resolve(&mut net, &mut rings, PEER_IP, &mut rx, &mut tx, 4),
            Ok(None)
        );
    }

    #[test]
    fn client_resolve_ignores_reply_for_other_ip() {
        let mut reply = [0u8; 64];
        let len = arp_reply_frame(&mut reply);
        let mut net = MockNet::with_inbound(&reply[..len]);
        let mut region = [0u8; REGION_LEN];
        let mut rings = bind_rings(&mut region);
        let mut rx = [0u8; 256];
        let mut tx = [0u8; 256];
        // Resolving a different target: the cached reply binds PEER_IP,
        // not 10.0.2.99, so no match.
        assert_eq!(
            client().resolve(
                &mut net,
                &mut rings,
                Ipv4Addr::new(10, 0, 2, 99),
                &mut rx,
                &mut tx,
                4
            ),
            Ok(None)
        );
    }

    #[test]
    fn client_ping_confirms_matching_reply() {
        let payload = [0x10, 0x20, 0x30];
        let mut reply = [0u8; 128];
        let len = echo_reply_frame(0xABCD, 9, &payload, &mut reply);
        let mut net = MockNet::with_inbound(&reply[..len]);
        let mut region = [0u8; REGION_LEN];
        let mut rings = bind_rings(&mut region);
        let mut rx = [0u8; 256];
        let mut tx = [0u8; 256];
        let pinged = client()
            .ping(
                &mut net, &mut rings, PEER_MAC, PEER_IP, 0xABCD, 9, &payload, &mut rx, &mut tx, 4,
            )
            .expect("no error");
        assert!(pinged);

        // The transmitted frame is an ICMP echo request to the peer.
        let eth = EthernetFrame::parse(net.transmitted()).expect("request parses");
        assert_eq!(eth.destination, PEER_MAC);
        let (ip, _options, datagram) = Ipv4Header::parse(eth.payload).expect("ip parses");
        assert_eq!(ip.destination, PEER_IP);
        let echo = IcmpEcho::parse(icmp::IcmpContext::V4, datagram).expect("icmp parses");
        assert_eq!(echo.kind, icmp::EchoKind::Request);
        assert_eq!(echo.identifier, 0xABCD);
        assert_eq!(echo.sequence, 9);
    }

    #[test]
    fn client_ping_rejects_mismatched_sequence() {
        let payload = [0x10, 0x20, 0x30];
        let mut reply = [0u8; 128];
        let len = echo_reply_frame(0xABCD, 9, &payload, &mut reply);
        let mut net = MockNet::with_inbound(&reply[..len]);
        let mut region = [0u8; REGION_LEN];
        let mut rings = bind_rings(&mut region);
        let mut rx = [0u8; 256];
        let mut tx = [0u8; 256];
        // The reply carries sequence 9; we asked for sequence 10.
        let pinged = client()
            .ping(
                &mut net, &mut rings, PEER_MAC, PEER_IP, 0xABCD, 10, &payload, &mut rx, &mut tx, 4,
            )
            .expect("no error");
        assert!(!pinged);
    }

    #[test]
    fn client_parse_arp_reply_rejects_a_request() {
        // A broadcast ARP *request* is addressed to us (broadcast) but
        // is not a reply, so it never resolves an address.
        let frame = arp_request_frame();
        assert_eq!(client().parse_arp_reply(&frame, LOCAL_IP), None);
    }

    #[test]
    fn client_write_request_reports_output_too_small() {
        let mut tiny = [0u8; 8];
        assert_eq!(
            client().write_arp_request(PEER_IP, &mut tiny),
            Err(NetServiceError::OutputTooSmall)
        );
        assert_eq!(
            client().write_echo_request(PEER_MAC, PEER_IP, 1, 1, &[0; 4], &mut tiny),
            Err(NetServiceError::OutputTooSmall)
        );
    }

    #[test]
    fn client_resolve_propagates_driver_error() {
        struct FaultyTx;
        impl Net for FaultyTx {
            fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
                Ok(local_facts())
            }
            fn service(
                &mut self,
                _rings: &mut FrameRings<'_>,
            ) -> Result<ServiceReport, DriverError> {
                Err(DriverError::Busy)
            }
        }
        let mut net = FaultyTx;
        let mut region = [0u8; REGION_LEN];
        let mut rings = bind_rings(&mut region);
        let mut rx = [0u8; 64];
        let mut tx = [0u8; 64];
        assert_eq!(
            client().resolve(&mut net, &mut rings, PEER_IP, &mut rx, &mut tx, 4),
            Err(NetServiceError::Driver(DriverError::Busy))
        );
    }
}
