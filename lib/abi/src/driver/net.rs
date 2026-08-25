//! Network driver class (`drivers/network/*`).
//!
//! Link-layer drivers send and receive raw Ethernet (or equivalent)
//! frames. Higher-layer protocols (IP, ARP, …) live above this
//! trait in user space and are out of scope for `abi-v1`.

use super::net_ring::{FrameRings, ServiceReport};
use super::DriverError;
use crate::Errno;

/// Length of an Ethernet MAC address.
pub const MAC_ADDRESS_LEN: usize = 6;

/// Octets an Ethernet II frame header occupies: destination MAC (6) +
/// source MAC (6) + `EtherType` (2). This is the fixed link-layer overhead a
/// link MTU excludes, so the largest frame a device moves is its MTU plus
/// this header. Defined once here and reused wherever a frame size is
/// derived from an MTU (the ring geometry bounds, the channel attach check).
pub const ETHERNET_HEADER_LEN: u32 = 14;

/// A 48-bit IEEE 802 link-layer address.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct MacAddress(pub [u8; MAC_ADDRESS_LEN]);

impl MacAddress {
    /// The all-ones link-layer broadcast address.
    pub const BROADCAST: Self = Self([0xFF; MAC_ADDRESS_LEN]);

    /// Construct a [`MacAddress`] from its six byte octets.
    #[must_use]
    pub const fn new(octets: [u8; MAC_ADDRESS_LEN]) -> Self {
        Self(octets)
    }

    /// Borrow the underlying byte slice.
    #[must_use]
    pub const fn as_octets(&self) -> &[u8; MAC_ADDRESS_LEN] {
        &self.0
    }
}

/// Current link state a network device reports.
///
/// The closed two-state vocabulary of [`DeviceFacts::link`]: a link is
/// either carrying frames or it is not. A device that cannot sense its
/// link (no status feature negotiated, no PHY report) reports [`Up`]
/// once it is operational — an unsensed link is not a third state the
/// stack could act on differently.
///
/// [`Up`]: LinkState::Up
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum LinkState {
    /// The link carries frames. An operational device that cannot sense
    /// its link reports this, so it is the default.
    #[default]
    Up,
    /// The link is down; transmits will not reach a peer.
    Down,
}

/// Closed set of hardware offload capabilities a network device has
/// **verified** it can perform (`plans/NETWORK.md` §2.3).
///
/// A `#[repr(transparent)]` newtype over the flag bits (the
/// [`crate::ipc::CallRecvFlags`] pattern): only the bits named here are
/// defined, every other bit is reserved, and [`NetOffloads::from_bits`]
/// rejects a value with any reserved bit set so an unknown claim is
/// refused rather than silently carried. The software path remains the
/// canonical implementation of every offloadable operation; a driver
/// advertises only what it implements and tests, and the stack opts in
/// per flag. No offload is ever load-bearing for security: a device
/// claim is trust in the *device*, never in the peer.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct NetOffloads(u32);

impl NetOffloads {
    /// Device computes the IPv4 header checksum on transmit.
    pub const TX_CSUM_IPV4: Self = Self(1 << 0);
    /// Device computes the TCP checksum on transmit (v4 and v6).
    pub const TX_CSUM_TCP: Self = Self(1 << 1);
    /// Device computes the UDP checksum on transmit (v4 and v6).
    pub const TX_CSUM_UDP: Self = Self(1 << 2);
    /// Device validates receive checksums and marks frames it verified.
    pub const RX_CSUM_VALIDATED: Self = Self(1 << 3);
    /// Device segments an over-size TCP payload against a template
    /// header on transmit (TSO-equivalent).
    pub const TX_SEGMENT_TCP: Self = Self(1 << 4);

    /// The set of all defined flag bits; anything else is reserved.
    const DEFINED_BITS: u32 = Self::TX_CSUM_IPV4.0
        | Self::TX_CSUM_TCP.0
        | Self::TX_CSUM_UDP.0
        | Self::RX_CSUM_VALIDATED.0
        | Self::TX_SEGMENT_TCP.0;

    /// No offloads: every operation runs on the canonical software path.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Raw flag bits, as carried on the ABI.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Build an offload set from raw bits, rejecting any reserved bit.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] if `bits` sets any reserved
    /// (currently-undefined) bit.
    pub const fn from_bits(bits: u32) -> Result<Self, Errno> {
        if bits & !Self::DEFINED_BITS != 0 {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(bits))
    }

    /// Whether every flag in `other` is present in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Typed report of a network device's facts (`plans/NETWORK.md` §2.3):
/// its link-layer address, MTU, live link state, verified offload set,
/// and receive-queue count.
///
/// The report describes the *device*, never the network: nothing in it
/// is attacker-controlled, but the stack still validates it whole
/// through [`DeviceFacts::validate`] before acting on it, because a
/// buggy driver is inside the fault boundary the stack defends (fail
/// closed, never "trusted driver").
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DeviceFacts {
    /// The device's 48-bit IEEE 802 link-layer address.
    pub mac: MacAddress,
    /// Link MTU: the largest link-layer *payload* (IP packet) in
    /// bytes, excluding the Ethernet header — 1500 for standard
    /// Ethernet. The largest frame the device moves is therefore
    /// `mtu` plus the 14-byte Ethernet header.
    pub mtu: u32,
    /// Current link state.
    pub link: LinkState,
    /// The offload capabilities the device verified it can perform.
    pub offloads: NetOffloads,
    /// Number of receive queues the device serves (`RX_MULTIQUEUE`).
    /// At least 1: a device with no receive queue is not a network
    /// device.
    pub rx_queues: u16,
}

impl DeviceFacts {
    /// Smallest link MTU the stack will drive: the RFC 791 §3.2
    /// 68-byte IPv4 reassembly floor. IPv6 additionally requires 1280
    /// (RFC 8200 §5), but that is a per-family policy the stack
    /// enforces when it binds v6 to the interface, not a device fact.
    pub const MIN_MTU: u32 = 68;

    /// Largest link MTU accepted: a jumbo-frame ceiling that bounds
    /// every buffer sized from this report, so a corrupt or hostile
    /// report can never induce an attacker-sized allocation.
    pub const MAX_MTU: u32 = 65_535;

    /// Validate the whole report, fail-closed.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] when `mtu` lies outside
    /// [`Self::MIN_MTU`]..=[`Self::MAX_MTU`] or `rx_queues` is zero.
    pub const fn validate(&self) -> Result<(), Errno> {
        if self.mtu < Self::MIN_MTU || self.mtu > Self::MAX_MTU || self.rx_queues == 0 {
            return Err(Errno::OutOfRange);
        }
        Ok(())
    }
}

/// Trait every link-layer network driver implements.
///
/// Frame I/O is the shared-memory frame-ring transport
/// (`plans/NETWORK.md` §2.3): the stack owns a [`FrameRings`] pair
/// (queued transmits in `tx`, delivered frames in `rx`) and hands it
/// to [`Net::service`], the single doorbell that moves frames both
/// ways. The driver mutates the rings only inside that call, so the
/// call boundary is the synchronisation and no frame bytes cross the
/// IPC when the rings live in a shared region.
///
/// # Capabilities
///
/// `service` is gated by
/// [`CapabilityId::NET_RAW`](crate::CapabilityId::NET_RAW) at the
/// dispatch site, on top of the load-time
/// [`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD) check.
/// `device_facts` is gated only by ownership of the
/// [`DriverHandle`](crate::driver::DriverHandle).
pub trait Net {
    /// Report the device's facts: link-layer address, MTU, link
    /// state, verified offload set, and receive-queue count.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the underlying hardware
    ///   could not be queried.
    ///
    /// # Capabilities
    ///
    /// Caller must present the driver's [`DriverHandle`].
    ///
    /// [`DriverHandle`]: crate::driver::DriverHandle
    fn device_facts(&self) -> Result<DeviceFacts, DriverError>;

    /// Service the frame rings once: move every frame queued in
    /// `rings.tx` into the device, then move delivered frames from
    /// the device into `rings.rx` until the device is drained or the
    /// ring is full ([`ServiceReport::rx_ring_full`] — nothing is
    /// dropped; the stack drains the ring and calls again).
    ///
    /// A frame whose length the device refuses (runt, over-MTU) is
    /// consumed from the TX ring and dropped — a malformed producer
    /// must not wedge the queue behind it — and is excluded from
    /// [`ServiceReport::transmitted`].
    ///
    /// When `rings.class` is
    /// [`BufferClass::Sensitive`](super::BufferClass::Sensitive) the
    /// driver zeroes every internal staging copy before returning.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if the dispatcher has not
    ///   verified `CAP_NET_RAW`.
    /// * [`DriverError::BadMagic`] if the ring state is corrupt (the
    ///   region's counters or a slot length fail validation).
    /// * [`DriverError::DeviceFault`] if the underlying hardware
    ///   failed.
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::NET_RAW`](crate::CapabilityId::NET_RAW).
    fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError>;

    /// Acknowledge the device's pending interrupt, deasserting the line.
    ///
    /// A user-space NIC driver parks on the device interrupt and, when it
    /// fires, must clear the device-level assertion **before** the line is
    /// re-enabled — otherwise the interrupt re-fires immediately and storms
    /// the driver off the run queue (the busy-wait the charter forbids). This
    /// is distinct from [`service`](Self::service): acknowledgement only
    /// clears the interrupt signal; the completed receive descriptors persist
    /// until the next `service` drains them, so a driver may acknowledge on
    /// the interrupt and defer the actual frame movement to the stack's next
    /// service doorbell.
    ///
    /// The default is a no-op for devices and back-ends that need no explicit
    /// acknowledgement (an in-process mock, a transport that self-clears);
    /// a real hardware device overrides it to clear its interrupt-status
    /// register.
    fn ack_interrupt(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::super::net_ring::RingGeometry;
    use super::super::BufferClass;
    use super::*;

    #[test]
    fn mac_address_round_trip() {
        let mac = MacAddress::new([0x02, 0xCA, 0xFE, 0xBA, 0xBE, 0x01]);
        assert_eq!(mac.as_octets()[5], 0x01);
    }

    #[test]
    fn offloads_from_bits_rejects_reserved_bits() {
        assert!(NetOffloads::from_bits(NetOffloads::TX_CSUM_IPV4.bits()).is_ok());
        assert_eq!(NetOffloads::from_bits(1 << 31), Err(Errno::OutOfRange));
        let set = NetOffloads::from_bits(
            NetOffloads::TX_CSUM_TCP.bits() | NetOffloads::RX_CSUM_VALIDATED.bits(),
        )
        .expect("defined bits");
        assert!(set.contains(NetOffloads::TX_CSUM_TCP));
        assert!(!set.contains(NetOffloads::TX_SEGMENT_TCP));
    }

    #[test]
    fn device_facts_validate_fails_closed() {
        let good = facts(MacAddress::new([2, 0, 0, 0, 0, 1]));
        assert!(good.validate().is_ok());
        let mut runt = good;
        runt.mtu = DeviceFacts::MIN_MTU - 1;
        assert_eq!(runt.validate(), Err(Errno::OutOfRange));
        let mut jumbo = good;
        jumbo.mtu = DeviceFacts::MAX_MTU + 1;
        assert_eq!(jumbo.validate(), Err(Errno::OutOfRange));
        let mut no_queue = good;
        no_queue.rx_queues = 0;
        assert_eq!(no_queue.validate(), Err(Errno::OutOfRange));
    }

    /// Test geometry: 4 slots of 128 bytes per ring (equal RX/TX), one
    /// receive queue.
    const GEOMETRY: RingGeometry = match RingGeometry::new(4, 128, 128, 1) {
        Ok(g) => g,
        Err(_) => panic!("valid test geometry"),
    };
    const REGION_LEN: usize = GEOMETRY.region_len();

    fn facts(mac: MacAddress) -> DeviceFacts {
        DeviceFacts {
            mac,
            mtu: 1500,
            link: LinkState::Up,
            offloads: NetOffloads::empty(),
            rx_queues: 1,
        }
    }

    /// A loopback [`Net`] mock: frames drained from the TX ring queue
    /// inside the device and come back on the RX ring, through a
    /// staging buffer that records whether it was scrubbed.
    struct MockNet {
        mac: MacAddress,
        pending: [([u8; 128], usize); 8],
        pending_len: usize,
        staging: [u8; 128],
        scrubbed_after_last_call: bool,
    }

    impl MockNet {
        fn new() -> Self {
            Self {
                mac: MacAddress::new([0; 6]),
                pending: [([0; 128], 0); 8],
                pending_len: 0,
                staging: [0; 128],
                scrubbed_after_last_call: false,
            }
        }
    }

    impl Net for MockNet {
        fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
            Ok(facts(self.mac))
        }

        fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
            let mut report = ServiceReport::default();
            self.scrubbed_after_last_call = false;
            // Drain the TX ring into the device's pending queue.
            loop {
                match rings.tx.pop(&mut self.staging) {
                    Ok(Some(len)) => {
                        // Device length policy: drop runts, count the rest.
                        if len >= 14 && self.pending_len < self.pending.len() {
                            self.pending[self.pending_len].0[..len]
                                .copy_from_slice(&self.staging[..len]);
                            self.pending[self.pending_len].1 = len;
                            self.pending_len += 1;
                            report.transmitted += 1;
                        }
                    }
                    Ok(None) => break,
                    // A corrupt slot was consumed; skip it and go on.
                    Err(Errno::LengthOutOfRange) => {}
                    Err(_) => return Err(DriverError::BadMagic),
                }
            }
            // Loop the pending frames back through the (single) RX ring.
            let mut delivered = 0;
            let rx0 = rings.rx_ring(0).map_err(|_| DriverError::BadMagic)?;
            for i in 0..self.pending_len {
                let (frame, len) = &self.pending[i];
                match rx0.push(&frame[..*len]) {
                    Ok(()) => {
                        report.received += 1;
                        delivered += 1;
                    }
                    Err(Errno::NoSpace) => {
                        report.rx_ring_full = true;
                        break;
                    }
                    Err(_) => return Err(DriverError::BadMagic),
                }
            }
            self.pending.copy_within(delivered..self.pending_len, 0);
            self.pending_len -= delivered;
            if rings.class.is_sensitive() {
                self.staging.fill(0);
                self.scrubbed_after_last_call = true;
            }
            Ok(report)
        }
    }

    #[test]
    fn loopback_round_trip() {
        let mut n = MockNet::new();
        let mut region = [0u8; REGION_LEN];
        let mut rings =
            FrameRings::bind(&mut region, GEOMETRY, BufferClass::NonSensitive).expect("bind");
        rings.tx.push(&[0xAA; 16]).expect("queue tx");
        let report = n.service(&mut rings).expect("service");
        assert_eq!(report.transmitted, 1);
        assert_eq!(report.received, 1);
        assert!(!report.rx_ring_full);
        let mut buf = [0u8; 128];
        assert_eq!(rings.rx_ring(0).expect("rx0").pop(&mut buf), Ok(Some(16)));
        assert_eq!(&buf[..16], &[0xAA; 16]);
        assert_eq!(rings.rx_ring(0).expect("rx0").pop(&mut buf), Ok(None));
    }

    #[test]
    fn runt_tx_frames_are_dropped_without_wedging() {
        let mut n = MockNet::new();
        let mut region = [0u8; REGION_LEN];
        let mut rings =
            FrameRings::bind(&mut region, GEOMETRY, BufferClass::NonSensitive).expect("bind");
        rings.tx.push(&[0x01; 4]).expect("queue runt");
        rings.tx.push(&[0x02; 20]).expect("queue good");
        let report = n.service(&mut rings).expect("service");
        // The runt was consumed and dropped; the good frame flowed.
        assert_eq!(report.transmitted, 1);
        assert_eq!(report.received, 1);
        let mut buf = [0u8; 128];
        assert_eq!(rings.rx_ring(0).expect("rx0").pop(&mut buf), Ok(Some(20)));
        assert_eq!(buf[0], 0x02);
    }

    #[test]
    fn rx_ring_full_backpressures_without_loss() {
        let mut n = MockNet::new();
        let mut region = [0u8; REGION_LEN];
        let mut rings =
            FrameRings::bind(&mut region, GEOMETRY, BufferClass::NonSensitive).expect("bind");
        // Five frames through a four-slot RX ring: two service passes.
        for _ in 0..4 {
            rings.tx.push(&[0x33; 20]).expect("queue");
        }
        let report = n.service(&mut rings).expect("service");
        assert_eq!(report.received, 4);
        rings.tx.push(&[0x44; 20]).expect("queue fifth");
        let report = n.service(&mut rings).expect("service");
        assert_eq!(report.transmitted, 1);
        assert!(report.rx_ring_full);
        assert_eq!(report.received, 0);
        // Drain and pump again: the fifth frame arrives, nothing lost.
        let mut buf = [0u8; 128];
        for _ in 0..4 {
            assert_eq!(rings.rx_ring(0).expect("rx0").pop(&mut buf), Ok(Some(20)));
        }
        let report = n.service(&mut rings).expect("service");
        assert_eq!(report.received, 1);
        assert_eq!(rings.rx_ring(0).expect("rx0").pop(&mut buf), Ok(Some(20)));
        assert_eq!(buf[0], 0x44);
    }

    #[test]
    fn sensitive_class_triggers_staging_scrub() {
        let mut n = MockNet::new();
        let mut region = [0u8; REGION_LEN];
        let mut rings =
            FrameRings::bind(&mut region, GEOMETRY, BufferClass::Sensitive).expect("bind");
        rings.tx.push(&[0xC3; 24]).expect("queue");
        n.service(&mut rings).expect("service");
        assert!(n.scrubbed_after_last_call);
        assert!(n.staging.iter().all(|&b| b == 0));
    }

    #[test]
    fn non_sensitive_class_leaves_staging() {
        let mut n = MockNet::new();
        let mut region = [0u8; REGION_LEN];
        let mut rings =
            FrameRings::bind(&mut region, GEOMETRY, BufferClass::NonSensitive).expect("bind");
        rings.tx.push(&[0xC3; 24]).expect("queue");
        n.service(&mut rings).expect("service");
        assert!(!n.scrubbed_after_last_call);
        assert!(n.staging.contains(&0xC3));
    }

    #[test]
    fn corrupt_ring_counters_fail_closed() {
        let mut n = MockNet::new();
        let mut region = [0u8; REGION_LEN];
        // Corrupt the TX ring's producer counter (second ring header).
        let tx_header = GEOMETRY.rx_ring_len();
        region[tx_header..tx_header + 4].copy_from_slice(&100u32.to_le_bytes());
        let mut rings =
            FrameRings::bind(&mut region, GEOMETRY, BufferClass::NonSensitive).expect("bind");
        assert_eq!(n.service(&mut rings), Err(DriverError::BadMagic));
    }
}
