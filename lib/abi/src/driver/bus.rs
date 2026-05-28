//! Bus driver class (`drivers/bus/*`).
//!
//! Bus drivers enumerate the devices attached to a transport (PCI,
//! MMIO, virtio). They do *not* speak to the device-class driver
//! sitting above them — the host wires the two together via
//! [`DriverHandle`](crate::driver::DriverHandle)s.

use super::DriverError;

/// Identifying tuple for a discovered device.
///
/// `vendor`, `device`, and `class` are bus-defined codes (PCI vendor/
/// device IDs, virtio device IDs, etc.). `address` is the bus-local
/// address (PCI BDF packed, MMIO physical address, virtio index).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct BusDevice {
    /// Bus-defined vendor identifier.
    pub vendor: u32,
    /// Bus-defined device identifier.
    pub device: u32,
    /// Bus-defined device-class code.
    pub class: u16,
    /// Reserved; must be zero in `abi-v1`.
    pub reserved0: u16,
    /// Bus-local address.
    pub address: u64,
}

/// Trait every bus driver implements.
///
/// # Capabilities
///
/// Enumeration is gated by ownership of the
/// [`DriverHandle`](crate::driver::DriverHandle) (load-time
/// [`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD)). The
/// host is expected to consult the audit trail
/// (`AGENTS.md` §5.4.4) before forwarding the enumerated devices to a
/// requester that does not itself hold `CAP_DRV_LOAD`.
pub trait Bus {
    /// Enumerate every device currently attached to the bus into
    /// `out`, returning the number of entries written.
    ///
    /// If `out.len()` is smaller than the actual device count the
    /// method writes as many entries as fit and returns
    /// [`DriverError::BufferTooSmall`] so the caller can resize and
    /// retry. The total device count is reachable through a second
    /// call with an `out.len()` of zero, which is the explicit "query
    /// length" form (returns `Ok(0)` if the bus is empty, else
    /// `Err(DriverError::BufferTooSmall)`; the caller queries again
    /// with a larger buffer).
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `out` cannot hold every
    ///   discovered device.
    /// * [`DriverError::DeviceFault`] if the bus transport reported
    ///   an unrecoverable enumeration error.
    ///
    /// # Capabilities
    ///
    /// Caller must present the driver's [`DriverHandle`].
    ///
    /// [`DriverHandle`]: crate::driver::DriverHandle
    fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockBus {
        devices: [BusDevice; 3],
    }

    impl Bus for MockBus {
        fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
            if out.len() < self.devices.len() {
                // Write what we can so a curious caller can see the
                // partial enumeration, but the canonical answer is
                // BufferTooSmall.
                let n = out.len();
                out[..n].copy_from_slice(&self.devices[..n]);
                return Err(DriverError::BufferTooSmall);
            }
            out[..self.devices.len()].copy_from_slice(&self.devices);
            Ok(self.devices.len())
        }
    }

    fn dev(addr: u64) -> BusDevice {
        BusDevice {
            vendor: 0x1AF4,
            device: 0x1000,
            class: 0x0200,
            reserved0: 0,
            address: addr,
        }
    }

    #[test]
    fn enumerate_returns_full_count() {
        let bus = MockBus {
            devices: [dev(1), dev(2), dev(3)],
        };
        let mut buf = [dev(0); 8];
        assert_eq!(bus.enumerate(&mut buf), Ok(3));
        assert_eq!(buf[0].address, 1);
        assert_eq!(buf[2].address, 3);
    }

    #[test]
    fn enumerate_signals_short_buffer() {
        let bus = MockBus {
            devices: [dev(1), dev(2), dev(3)],
        };
        let mut buf = [dev(0); 2];
        assert_eq!(bus.enumerate(&mut buf), Err(DriverError::BufferTooSmall));
    }
}
