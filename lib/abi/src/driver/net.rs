//! Network driver class (`drivers/network/*`).
//!
//! Link-layer drivers send and receive raw Ethernet (or equivalent)
//! frames. Higher-layer protocols (IP, ARP, …) live above this
//! trait in user space and are out of scope for `abi-v1`.

use super::{BufferClass, DriverError};
use crate::Errno;

/// Length of an Ethernet MAC address.
pub const MAC_ADDRESS_LEN: usize = 6;

/// A 48-bit IEEE 802 link-layer address.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct MacAddress(pub [u8; MAC_ADDRESS_LEN]);

impl MacAddress {
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
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum LinkState {
    /// The link carries frames.
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
/// # Capabilities
///
/// `transmit` and `receive` are gated by
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

    /// Transmit one frame.
    ///
    /// The frame is consumed by the device when this method returns
    /// `Ok(())`; the caller may reuse the buffer.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if the dispatcher has not
    ///   verified `CAP_NET_RAW`.
    /// * [`DriverError::BufferTooSmall`] if `frame.len()` is shorter
    ///   than the minimum link-layer frame size for the device.
    /// * [`DriverError::LengthOutOfRange`] if `frame.len()` exceeds
    ///   the device MTU.
    /// * [`DriverError::Busy`] if the transmit queue is full.
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::NET_RAW`](crate::CapabilityId::NET_RAW).
    fn transmit(&mut self, frame: &[u8]) -> Result<(), DriverError>;

    /// Drain one frame into `buf`, returning the number of bytes
    /// written.
    ///
    /// Returns `Ok(0)` when no frame is available; `buf` is left
    /// untouched in that case.
    ///
    /// # Errors
    ///
    /// * [`DriverError::PermissionDenied`] if the dispatcher has not
    ///   verified `CAP_NET_RAW`.
    /// * [`DriverError::BufferTooSmall`] if the next pending frame is
    ///   longer than `buf`.
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::NET_RAW`](crate::CapabilityId::NET_RAW).
    fn receive(&mut self, buf: &mut [u8]) -> Result<usize, DriverError>;

    /// Transmit one frame, declaring the payload's sensitivity class.
    ///
    /// Behaviour is identical to [`Self::transmit`] except that when
    /// `class == BufferClass::Sensitive` the driver is required to
    /// zero every internal staging copy of the frame before this
    /// method returns.
    ///
    /// The default implementation delegates to [`Self::transmit`]
    /// and is only safe for drivers that DMA straight from `frame`
    /// without retaining a private copy. Drivers that bounce-buffer
    /// transmits **must** override.
    ///
    /// # Errors
    ///
    /// As for [`Self::transmit`].
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::NET_RAW`](crate::CapabilityId::NET_RAW).
    fn transmit_with_class(&mut self, frame: &[u8], class: BufferClass) -> Result<(), DriverError> {
        let _ = class;
        self.transmit(frame)
    }

    /// Drain one frame, declaring the receive buffer's sensitivity
    /// class.
    ///
    /// Behaviour is identical to [`Self::receive`] except that when
    /// `class == BufferClass::Sensitive` the driver is required to
    /// zero every internal staging copy of the frame before this
    /// method returns. The caller-owned `buf` is left populated; it
    /// is the caller's responsibility to scrub `buf` once the
    /// payload is consumed.
    ///
    /// The default implementation delegates to [`Self::receive`]
    /// and is only safe for drivers that DMA straight into `buf`.
    /// Drivers that bounce-buffer receives **must** override.
    ///
    /// # Errors
    ///
    /// As for [`Self::receive`].
    ///
    /// # Capabilities
    ///
    /// Requires [`CapabilityId::NET_RAW`](crate::CapabilityId::NET_RAW).
    fn receive_with_class(
        &mut self,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<usize, DriverError> {
        let _ = class;
        self.receive(buf)
    }
}

#[cfg(test)]
mod tests {
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

    struct MockNet {
        mac: MacAddress,
        last_tx: [u8; 64],
        last_tx_len: usize,
    }

    fn facts(mac: MacAddress) -> DeviceFacts {
        DeviceFacts {
            mac,
            mtu: 1500,
            link: LinkState::Up,
            offloads: NetOffloads::empty(),
            rx_queues: 1,
        }
    }

    impl Net for MockNet {
        fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
            Ok(facts(self.mac))
        }

        fn transmit(&mut self, frame: &[u8]) -> Result<(), DriverError> {
            if frame.len() < 14 {
                return Err(DriverError::BufferTooSmall);
            }
            if frame.len() > self.last_tx.len() {
                return Err(DriverError::LengthOutOfRange);
            }
            self.last_tx[..frame.len()].copy_from_slice(frame);
            self.last_tx_len = frame.len();
            Ok(())
        }

        fn receive(&mut self, buf: &mut [u8]) -> Result<usize, DriverError> {
            if self.last_tx_len == 0 {
                return Ok(0);
            }
            if buf.len() < self.last_tx_len {
                return Err(DriverError::BufferTooSmall);
            }
            buf[..self.last_tx_len].copy_from_slice(&self.last_tx[..self.last_tx_len]);
            let n = self.last_tx_len;
            self.last_tx_len = 0;
            Ok(n)
        }
    }

    #[test]
    fn loopback_round_trip() {
        let mut n = MockNet {
            mac: MacAddress::new([0; 6]),
            last_tx: [0; 64],
            last_tx_len: 0,
        };
        let frame = [0xAAu8; 16];
        assert!(n.transmit(&frame).is_ok());
        let mut buf = [0u8; 64];
        assert_eq!(n.receive(&mut buf), Ok(16));
        assert_eq!(&buf[..16], &frame[..]);
        assert_eq!(n.receive(&mut buf), Ok(0));
    }

    /// `Net` impl that stages tx/rx through a private buffer and
    /// scrubs the staging on Sensitive.
    struct SensitiveStagingNet {
        mac: MacAddress,
        staging: [u8; 64],
        staged_len: usize,
        scrubbed_after_last_call: bool,
    }

    impl Net for SensitiveStagingNet {
        fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
            Ok(facts(self.mac))
        }
        fn transmit(&mut self, frame: &[u8]) -> Result<(), DriverError> {
            if frame.len() < 14 {
                return Err(DriverError::BufferTooSmall);
            }
            if frame.len() > self.staging.len() {
                return Err(DriverError::LengthOutOfRange);
            }
            self.staging[..frame.len()].copy_from_slice(frame);
            self.staged_len = frame.len();
            self.scrubbed_after_last_call = false;
            Ok(())
        }
        fn receive(&mut self, buf: &mut [u8]) -> Result<usize, DriverError> {
            if self.staged_len == 0 {
                return Ok(0);
            }
            if buf.len() < self.staged_len {
                return Err(DriverError::BufferTooSmall);
            }
            buf[..self.staged_len].copy_from_slice(&self.staging[..self.staged_len]);
            let n = self.staged_len;
            self.staged_len = 0;
            self.scrubbed_after_last_call = false;
            Ok(n)
        }
        fn transmit_with_class(
            &mut self,
            frame: &[u8],
            class: BufferClass,
        ) -> Result<(), DriverError> {
            self.transmit(frame)?;
            if class.is_sensitive() {
                self.staging.fill(0);
                self.scrubbed_after_last_call = true;
            }
            Ok(())
        }
        fn receive_with_class(
            &mut self,
            buf: &mut [u8],
            class: BufferClass,
        ) -> Result<usize, DriverError> {
            let n = self.receive(buf)?;
            if class.is_sensitive() {
                self.staging.fill(0);
                self.scrubbed_after_last_call = true;
            }
            Ok(n)
        }
    }

    #[test]
    fn net_sensitive_class_triggers_staging_scrub() {
        let mut n = SensitiveStagingNet {
            mac: MacAddress::new([0; 6]),
            staging: [0; 64],
            staged_len: 0,
            scrubbed_after_last_call: false,
        };
        let frame = [0xC3u8; 24];
        assert!(n
            .transmit_with_class(&frame, BufferClass::Sensitive)
            .is_ok());
        assert!(n.scrubbed_after_last_call);
        assert!(n.staging.iter().all(|b| *b == 0));
        // Reload staging by transmitting non-sensitively then receiving
        // non-sensitively; staging must NOT be wiped.
        assert!(n
            .transmit_with_class(&frame, BufferClass::NonSensitive)
            .is_ok());
        let mut buf = [0u8; 64];
        assert_eq!(
            n.receive_with_class(&mut buf, BufferClass::NonSensitive),
            Ok(24)
        );
        assert!(!n.scrubbed_after_last_call);
        assert!(n.staging.contains(&0xC3));
    }

    #[test]
    fn net_default_with_class_delegates() {
        let mut n = MockNet {
            mac: MacAddress::new([0; 6]),
            last_tx: [0; 64],
            last_tx_len: 0,
        };
        let frame = [0xABu8; 20];
        assert!(n
            .transmit_with_class(&frame, BufferClass::NonSensitive)
            .is_ok());
        let mut buf = [0u8; 64];
        assert_eq!(
            n.receive_with_class(&mut buf, BufferClass::NonSensitive),
            Ok(20)
        );
        assert_eq!(&buf[..20], &frame[..]);
    }

    #[test]
    fn transmit_rejects_runt() {
        let mut n = MockNet {
            mac: MacAddress::new([0; 6]),
            last_tx: [0; 64],
            last_tx_len: 0,
        };
        let frame = [0u8; 4];
        assert_eq!(n.transmit(&frame), Err(DriverError::BufferTooSmall));
    }
}
