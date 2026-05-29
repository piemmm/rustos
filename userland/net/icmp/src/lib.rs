//! Userland ARP / IPv4 / ICMP-echo responder (`userland/net/icmp`).
//!
//! This crate is the smallest network service RustOS ships: it answers
//! ARP requests for a single configured IPv4 address and replies to
//! ICMP echo requests ("ping") aimed at that address. It is the
//! protocol peer the virtio-net QEMU integration tests exercise
//! (`PLAN.md` Stage 4.D).
//!
//! # Scope
//!
//! In scope: ARP request + reply (RFC 826), IPv4 (RFC 791, option-free
//! headers only), and ICMP echo (RFC 792). Explicitly **out of scope**
//! and deferred to Stage 6: TCP, UDP, IPv6, IP routing, fragmentation,
//! and any form of neighbour cache or retransmission.
//!
//! # Design
//!
//! The crate is `no_std` and allocation-free. [`Responder`] holds the
//! interface's link-layer and IPv4 addresses and is otherwise
//! stateless: [`Responder::handle_frame`] is a pure function from an
//! inbound frame plus a caller-owned scratch buffer to an optional
//! outbound frame. [`Responder::poll`] and [`Responder::run`] drive
//! that logic over any [`Net`] driver, so the same code runs against a
//! real virtio-net device and against a mock in unit tests.
//!
//! # Security
//!
//! The responder performs no privileged operation itself; it only
//! transforms bytes. Capability enforcement for [`Net::transmit`] /
//! [`Net::receive`] (`CAP_NET_RAW`) happens at the driver dispatch
//! site (`AGENTS.md` §5.4), upstream of this crate. Frames addressed
//! to neither this host nor the broadcast address are ignored, and a
//! reply is only emitted for a request that is well-formed, correctly
//! addressed, and (for ICMP) checksum-valid; everything else is
//! dropped silently rather than answered.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod arp;
pub mod ethernet;
pub mod icmp;
pub mod ipv4;

use rustos_abi::driver::net::{MacAddress, Net};
use rustos_abi::DriverError;

use crate::arp::ArpPacket;
use crate::ethernet::EthernetFrame;
use crate::icmp::IcmpEcho;
use crate::ipv4::Ipv4Header;

/// A 32-bit IPv4 address in network byte order.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Ipv4Address(pub [u8; 4]);

impl Ipv4Address {
    /// Construct an address from its four octets.
    #[must_use]
    pub const fn new(octets: [u8; 4]) -> Self {
        Self(octets)
    }

    /// Borrow the underlying octets.
    #[must_use]
    pub const fn as_octets(&self) -> &[u8; 4] {
        &self.0
    }
}

/// Error returned while servicing a frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NetServiceError {
    /// The underlying [`Net`] driver returned an error.
    Driver(DriverError),
    /// The supplied transmit buffer was too small to hold the reply.
    OutputTooSmall,
}

impl From<DriverError> for NetServiceError {
    fn from(error: DriverError) -> Self {
        Self::Driver(error)
    }
}

/// Stateless ARP + ICMP-echo responder for one network interface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Responder {
    mac: MacAddress,
    ip: Ipv4Address,
}

impl Responder {
    /// Bind the responder to an interface's addresses.
    #[must_use]
    pub const fn new(mac: MacAddress, ip: Ipv4Address) -> Self {
        Self { mac, ip }
    }

    /// The link-layer address this responder answers for.
    #[must_use]
    pub const fn mac_address(&self) -> MacAddress {
        self.mac
    }

    /// The IPv4 address this responder answers for.
    #[must_use]
    pub const fn ipv4_address(&self) -> Ipv4Address {
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

    /// Run a single receive/answer/transmit cycle over `net`.
    ///
    /// Returns `Ok(true)` when a frame was received and processed (it
    /// may or may not have warranted a reply) and `Ok(false)` when no
    /// frame was pending. `rx` is the scratch buffer frames are received
    /// into and `tx` the buffer replies are assembled in.
    pub fn poll<N: Net>(
        &self,
        net: &mut N,
        rx: &mut [u8],
        tx: &mut [u8],
    ) -> Result<bool, NetServiceError> {
        let received = net.receive(rx)?;
        if received == 0 {
            return Ok(false);
        }
        if let Some(len) = self.handle_frame(&rx[..received], tx)? {
            net.transmit(&tx[..len])?;
        }
        Ok(true)
    }

    /// Poll `net` up to `max_polls` times, returning how many frames
    /// were received and processed.
    ///
    /// The bound keeps the loop finite for tests and for callers that
    /// interleave other work; a long-running service passes its own
    /// budget and re-enters between blocking waits on the driver.
    pub fn run<N: Net>(
        &self,
        net: &mut N,
        rx: &mut [u8],
        tx: &mut [u8],
        max_polls: usize,
    ) -> Result<usize, NetServiceError> {
        let mut handled = 0;
        for _ in 0..max_polls {
            if self.poll(net, rx, tx)? {
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
        let eth_len = ethernet::write_header(
            out,
            request.sender_hardware,
            self.mac,
            ethernet::ETHERTYPE_ARP,
        )
        .ok_or(NetServiceError::OutputTooSmall)?;
        let body = out
            .get_mut(eth_len..)
            .ok_or(NetServiceError::OutputTooSmall)?;
        let arp_len = reply.write(body).ok_or(NetServiceError::OutputTooSmall)?;
        Ok(Some(eth_len + arp_len))
    }

    fn answer_ipv4(
        &self,
        payload: &[u8],
        peer_mac: MacAddress,
        out: &mut [u8],
    ) -> Result<Option<usize>, NetServiceError> {
        let Some((header, datagram)) = Ipv4Header::parse(payload) else {
            return Ok(None);
        };
        if header.protocol != ipv4::PROTOCOL_ICMP || header.destination != self.ip {
            return Ok(None);
        }
        let Some(request) = IcmpEcho::parse(datagram) else {
            return Ok(None);
        };
        if request.message_type != icmp::TYPE_ECHO_REQUEST {
            return Ok(None);
        }
        let echo = request.reply();
        let reply_header = Ipv4Header {
            source: self.ip,
            destination: header.source,
            protocol: ipv4::PROTOCOL_ICMP,
        };
        let len = self.frame_icmp(&reply_header, &echo, peer_mac, out)?;
        Ok(Some(len))
    }

    fn frame_icmp(
        &self,
        ip_header: &Ipv4Header,
        echo: &IcmpEcho<'_>,
        peer_mac: MacAddress,
        out: &mut [u8],
    ) -> Result<usize, NetServiceError> {
        let eth_len = ethernet::write_header(out, peer_mac, self.mac, ethernet::ETHERTYPE_IPV4)
            .ok_or(NetServiceError::OutputTooSmall)?;
        let icmp_len = echo.wire_len();
        let icmp_start = eth_len + ipv4::IPV4_HEADER_LEN;
        let icmp_buf = out
            .get_mut(icmp_start..)
            .ok_or(NetServiceError::OutputTooSmall)?;
        echo.write(icmp_buf)
            .ok_or(NetServiceError::OutputTooSmall)?;
        let ip_buf = out
            .get_mut(eth_len..)
            .ok_or(NetServiceError::OutputTooSmall)?;
        ip_header
            .write(ip_buf, icmp_len)
            .ok_or(NetServiceError::OutputTooSmall)?;
        Ok(eth_len + ipv4::IPV4_HEADER_LEN + icmp_len)
    }
}

/// Compute the 16-bit one's-complement Internet checksum (RFC 1071)
/// over `data`.
///
/// Shared by [`ipv4`] and [`icmp`] so the fold is written once. A
/// trailing odd byte is treated as the high byte of a final 16-bit
/// word, matching the algorithm every checksummed header on the wire
/// uses.
pub(crate) fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut words = data.chunks_exact(2);
    for word in &mut words {
        sum += u32::from(u16::from_be_bytes([word[0], word[1]]));
    }
    if let [last] = words.remainder() {
        sum += u32::from(*last) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !((sum & 0xFFFF) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arp::ArpPacket;
    use crate::icmp::IcmpEcho;

    const LOCAL_MAC: MacAddress = MacAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    const LOCAL_IP: Ipv4Address = Ipv4Address([10, 0, 2, 15]);
    const PEER_MAC: MacAddress = MacAddress([0x02, 0xCA, 0xFE, 0xBA, 0xBE, 0x01]);
    const PEER_IP: Ipv4Address = Ipv4Address([10, 0, 2, 2]);

    fn responder() -> Responder {
        Responder::new(LOCAL_MAC, LOCAL_IP)
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

    impl Net for MockNet {
        fn mac_address(&self) -> Result<MacAddress, DriverError> {
            Ok(LOCAL_MAC)
        }

        fn transmit(&mut self, frame: &[u8]) -> Result<(), DriverError> {
            if frame.len() > self.tx.len() {
                return Err(DriverError::LengthOutOfRange);
            }
            self.tx[..frame.len()].copy_from_slice(frame);
            self.tx_len = frame.len();
            Ok(())
        }

        fn receive(&mut self, buf: &mut [u8]) -> Result<usize, DriverError> {
            if self.delivered || self.rx_len == 0 {
                return Ok(0);
            }
            if buf.len() < self.rx_len {
                return Err(DriverError::BufferTooSmall);
            }
            buf[..self.rx_len].copy_from_slice(&self.rx[..self.rx_len]);
            self.delivered = true;
            Ok(self.rx_len)
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
            message_type: icmp::TYPE_ECHO_REQUEST,
            identifier: 0xBEEF,
            sequence: 7,
            payload,
        };
        let eth = ethernet::write_header(out, LOCAL_MAC, PEER_MAC, ethernet::ETHERTYPE_IPV4)
            .expect("eth header fits");
        let icmp_len = echo.wire_len();
        echo.write(&mut out[eth + ipv4::IPV4_HEADER_LEN..])
            .expect("icmp fits");
        Ipv4Header {
            source: PEER_IP,
            destination: LOCAL_IP,
            protocol: ipv4::PROTOCOL_ICMP,
        }
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
        let (ip, datagram) = Ipv4Header::parse(eth.payload).expect("ip parses");
        assert_eq!(ip.source, LOCAL_IP);
        assert_eq!(ip.destination, PEER_IP);
        let echo = IcmpEcho::parse(datagram).expect("icmp parses");
        assert_eq!(echo.message_type, icmp::TYPE_ECHO_REPLY);
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
        let mut rx = [0u8; 256];
        let mut tx = [0u8; 256];
        assert_eq!(responder().poll(&mut net, &mut rx, &mut tx), Ok(true));
        let eth = EthernetFrame::parse(net.transmitted()).expect("reply parses");
        assert_eq!(eth.ethertype, ethernet::ETHERTYPE_ARP);
        // A second poll finds nothing pending.
        assert_eq!(responder().poll(&mut net, &mut rx, &mut tx), Ok(false));
    }

    #[test]
    fn poll_on_empty_driver_is_idle() {
        let mut net = MockNet::empty();
        let mut rx = [0u8; 64];
        let mut tx = [0u8; 64];
        assert_eq!(responder().poll(&mut net, &mut rx, &mut tx), Ok(false));
        assert!(net.transmitted().is_empty());
    }

    #[test]
    fn run_counts_processed_frames() {
        let frame = arp_request_frame();
        let mut net = MockNet::with_inbound(&frame);
        let mut rx = [0u8; 256];
        let mut tx = [0u8; 256];
        assert_eq!(responder().run(&mut net, &mut rx, &mut tx, 4), Ok(1));
    }

    #[test]
    fn poll_propagates_driver_error() {
        // A driver whose receive always faults surfaces as Driver(_).
        struct FaultyNet;
        impl Net for FaultyNet {
            fn mac_address(&self) -> Result<MacAddress, DriverError> {
                Ok(LOCAL_MAC)
            }
            fn transmit(&mut self, _frame: &[u8]) -> Result<(), DriverError> {
                Ok(())
            }
            fn receive(&mut self, _buf: &mut [u8]) -> Result<usize, DriverError> {
                Err(DriverError::DeviceFault)
            }
        }
        let mut net = FaultyNet;
        let mut rx = [0u8; 64];
        let mut tx = [0u8; 64];
        assert_eq!(
            responder().poll(&mut net, &mut rx, &mut tx),
            Err(NetServiceError::Driver(DriverError::DeviceFault))
        );
    }

    #[test]
    fn checksum_of_known_vector() {
        // RFC 1071 worked example.
        let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(internet_checksum(&data), 0x220d);
    }

    #[test]
    fn addresses_round_trip() {
        let r = responder();
        assert_eq!(r.mac_address(), LOCAL_MAC);
        assert_eq!(r.ipv4_address(), LOCAL_IP);
        assert_eq!(Ipv4Address::new([1, 2, 3, 4]).as_octets(), &[1, 2, 3, 4]);
    }
}
