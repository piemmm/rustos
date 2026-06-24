//! Network driver class (`drivers/network/*`).
//!
//! Link-layer drivers send and receive raw Ethernet (or equivalent)
//! frames. Higher-layer protocols (IP, ARP, …) live above this
//! trait in user space and are out of scope for `abi-v1`.

use super::{BufferClass, DriverError};

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

/// Trait every link-layer network driver implements.
///
/// # Capabilities
///
/// `transmit` and `receive` are gated by
/// [`CapabilityId::NET_RAW`](crate::CapabilityId::NET_RAW) at the
/// dispatch site, on top of the load-time
/// [`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD) check.
/// `mac_address` is gated only by ownership of the
/// [`DriverHandle`](crate::driver::DriverHandle).
pub trait Net {
    /// Report the device's link-layer address.
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
    fn mac_address(&self) -> Result<MacAddress, DriverError>;

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

    struct MockNet {
        mac: MacAddress,
        last_tx: [u8; 64],
        last_tx_len: usize,
    }

    impl Net for MockNet {
        fn mac_address(&self) -> Result<MacAddress, DriverError> {
            Ok(self.mac)
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
        fn mac_address(&self) -> Result<MacAddress, DriverError> {
            Ok(self.mac)
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
                for byte in &mut self.staging {
                    *byte = 0;
                }
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
                for byte in &mut self.staging {
                    *byte = 0;
                }
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
