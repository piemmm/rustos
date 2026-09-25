//! [`Transport`] trait.
//!
//! A [`Transport`] is the bus-arch-specific seam every virtio driver
//! talks to. The trait defines the minimum surface required to bring
//! a split-virtqueue device online per virtio 1.1 §3.1:
//!
//! 1. Reset → acknowledge.
//! 2. Feature negotiation (driver-features bitmap).
//! 3. Set `FEATURES_OK`; abort if the device clears it.
//! 4. Per-queue programming (`queue_select` → `queue_set`).
//! 5. Set `DRIVER_OK`.
//! 6. Notify a queue when a chain is published.
//!
//! The in-process `MockTransport` peer the tests drive is built only for
//! tests, behind the crate's `mock` feature.

use tairix_abi::{DriverError, RegisterWindow};

#[cfg(any(test, feature = "mock"))]
mod mock;
#[cfg(any(test, feature = "mock"))]
pub use mock::{ChainView, DeviceShim, MockTransport};

/// Virtio device-status bits.
///
/// Mirrors virtio 1.1 §2.1; the wire layout is the device-status
/// byte at offset `+0x12` of the legacy-PCI common register window
/// and at offset `+0x70` of the modern common-cfg window. The
/// constants are repeated rather than depending on a vendored
/// virtio crate to keep the crate's transitive dependency surface
/// equal to its parent bus drivers.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct Status(u8);

impl Status {
    /// Indicates that the guest OS has found the device and
    /// recognised it as a valid virtio device.
    pub const ACKNOWLEDGE: u8 = 1;
    /// Indicates that the guest OS knows how to drive the device.
    pub const DRIVER: u8 = 2;
    /// Indicates that the driver is set up and ready to drive the
    /// device.
    pub const DRIVER_OK: u8 = 4;
    /// Indicates that the driver has acknowledged the feature
    /// negotiation result and is happy to proceed.
    pub const FEATURES_OK: u8 = 8;
    /// Indicates that the device has experienced an error from
    /// which it cannot recover.
    pub const DEVICE_NEEDS_RESET: u8 = 64;
    /// Indicates that something went wrong in the guest, and it has
    /// given up on the device.
    pub const FAILED: u8 = 128;

    /// Wrap an explicit byte value.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }
    /// Raw on-wire byte.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }
    /// `true` iff every bit in `mask` is set.
    #[must_use]
    pub const fn contains(self, mask: u8) -> bool {
        (self.0 & mask) == mask
    }
    /// Return a new `Status` with `mask` bits added.
    #[must_use]
    pub const fn with(self, mask: u8) -> Self {
        Self(self.0 | mask)
    }
}

/// Status reads a reset may take to confirm before the device counts as
/// wedged.
const RESET_POLL_BUDGET: u32 = 1_000_000;

/// Wait for a reset device's status to read 0, as virtio 1.1 §2.4.1 requires
/// before the driver may re-initialise it. `read_status` is `None` when the
/// register cannot be read at all.
pub(crate) fn await_reset(read_status: impl FnMut() -> Option<u32>) -> Result<(), VirtioError> {
    await_reset_within(RESET_POLL_BUDGET, read_status)
}

fn await_reset_within(
    budget: u32,
    mut read_status: impl FnMut() -> Option<u32>,
) -> Result<(), VirtioError> {
    for _ in 0..budget {
        match read_status() {
            Some(0) => return Ok(()),
            Some(_) => core::hint::spin_loop(),
            None => return Err(VirtioError::DeviceFault),
        }
    }
    Err(VirtioError::DeviceFault)
}

/// Errors a transport may return on its setup path.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum VirtioError {
    /// The device cleared [`Status::FEATURES_OK`] after the driver
    /// finished negotiation, i.e. the device rejected the chosen
    /// feature subset.
    FeaturesRejected,
    /// The driver asked the transport for a queue index outside the
    /// number of queues the device advertises.
    QueueIndexOutOfRange,
    /// The driver tried to program a queue with `size` greater than
    /// [`Transport::queue_max_size`].
    QueueSizeTooLarge,
    /// The device's queue cannot hold every descriptor the driver keeps on it
    /// at once.
    QueueTooShallow,
    /// A descriptor chain exceeds the queue size.
    DescriptorTableOverflow,
    /// The free-descriptor pool is empty.
    QueueFull,
    /// No used-ring entry available yet.
    NoCompletion,
    /// A device-written used-ring completion named a descriptor head
    /// outside the granted descriptor table (of the
    /// security charter, CWE-1257 / Thunderclap-class). The driver
    /// rejects it fail-closed rather than dereference a
    /// descriptor index that escapes the region.
    MalformedCompletion,
    /// The device reported a transport-level fault on the wire.
    DeviceFault,
}

impl VirtioError {
    /// Map a transport-level error onto the stable
    /// [`DriverError`] surface that crosses the driver-class trait
    /// boundary.
    #[must_use]
    pub const fn as_driver_error(self) -> DriverError {
        match self {
            Self::FeaturesRejected
            | Self::DeviceFault
            | Self::DescriptorTableOverflow
            | Self::MalformedCompletion => DriverError::DeviceFault,
            Self::QueueIndexOutOfRange | Self::QueueSizeTooLarge => DriverError::OutOfRange,
            Self::QueueTooShallow => DriverError::Unsupported,
            Self::QueueFull | Self::NoCompletion => DriverError::Busy,
        }
    }
}

/// Split a 64-bit register value into the low and high `u32` halves the
/// device takes it as.
///
/// virtio defines every 64-bit register as a pair of 32-bit accesses
/// (virtio 1.1 §4.1.3.1, §4.2.2), so both transports address the halves
/// rather than the whole.
pub(crate) fn le_halves(value: u64) -> (u32, u32) {
    ((value & 0xFFFF_FFFF) as u32, (value >> 32) as u32)
}

/// Reassemble a 64-bit register value from the halves the device
/// reports, the inverse of [`le_halves`].
pub(crate) fn u64_from_le_halves(low: u32, high: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

/// Write `value` to the 64-bit register whose low half sits at
/// `low_offset`, low half first as virtio requires.
pub(crate) fn write_u64_halves(
    window: &RegisterWindow,
    low_offset: usize,
    value: u64,
) -> Result<(), VirtioError> {
    let (low, high) = le_halves(value);
    window
        .write_u32(low_offset, low)
        .map_err(|_| VirtioError::DeviceFault)?;
    window
        .write_u32(low_offset + 4, high)
        .map_err(|_| VirtioError::DeviceFault)
}

/// Direction of a descriptor in a virtqueue chain.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Direction {
    /// Device reads from this buffer (driver-to-device).
    DeviceRead,
    /// Device writes into this buffer (device-to-driver).
    DeviceWrite,
}

/// Bus-arch-specific seam every virtio driver talks to.
///
/// Methods are sequenced as virtio 1.1 §3.1. Errors are returned
/// rather than swallowed so a driver can surface
/// [`DriverError::DeviceFault`] / [`DriverError::OutOfRange`]
/// rather than panic.
pub trait Transport {
    /// Reset the device and wait for it to confirm, after which it no longer
    /// reaches any memory it was given.
    ///
    /// # Errors
    ///
    /// [`VirtioError::DeviceFault`] if the device never confirms: it may still
    /// be mastering that memory, so none of it may be released.
    fn reset(&mut self) -> Result<(), VirtioError>;
    /// Read the device's current status byte.
    fn status(&self) -> Status;
    /// Write `status` to the device's status register.
    fn set_status(&mut self, status: Status);

    /// Read the device-features bitmap the device advertises
    /// (low 64 bits — Stage 4 does not negotiate extended features).
    fn device_features(&self) -> u64;
    /// Write the driver-features bitmap.
    fn set_driver_features(&mut self, features: u64);

    /// Number of virtqueues the device implements.
    fn num_queues(&self) -> u16;
    /// Select the active queue for the next `queue_*` operation.
    ///
    /// # Errors
    ///
    /// * [`VirtioError::QueueIndexOutOfRange`] if `queue >=
    ///   self.num_queues()`.
    fn queue_select(&mut self, queue: u16) -> Result<(), VirtioError>;
    /// Maximum queue size the device supports for the currently
    /// selected queue.
    fn queue_max_size(&self) -> u16;
    /// Program the currently-selected queue with its descriptor /
    /// avail / used physical addresses and `size`.
    ///
    /// # Errors
    ///
    /// * [`VirtioError::QueueSizeTooLarge`] if `size >
    ///   self.queue_max_size()`.
    fn queue_set(&mut self, size: u16, desc: u64, avail: u64, used: u64)
        -> Result<(), VirtioError>;

    /// Notify the device that the driver published new chain(s) on
    /// `queue`.
    fn notify(&mut self, queue: u16);

    /// Read `buf.len()` bytes from the device-configuration area
    /// starting at byte `offset`.
    fn read_config(&self, offset: usize, buf: &mut [u8]);

    /// Acknowledge the device's interrupt after the driver has consumed
    /// the completions it signalled, so the device de-asserts its line.
    ///
    /// This is the **device-level** half of interrupt handling, distinct
    /// from the GIC/APIC-level acknowledge the kernel's IRQ dispatch does:
    /// after the driver drains the used ring for a completion, it must tell
    /// the device it has handled the notification, or the device keeps the
    /// interrupt asserted and the *next* unmask re-delivers the same stale
    /// edge — corrupting back-to-back requests. A driver calls this once
    /// per `notify_wait` + drain cycle.
    ///
    /// The default is a no-op: it is correct for transports that need no
    /// explicit device-side acknowledge — MSI-X PCI (each completion is a
    /// fresh edge with no shared status to clear) and the in-process
    /// `MockTransport` (no real device). The modern **MMIO** transport
    /// overrides it to read `InterruptStatus` and write the handled bits
    /// back to `InterruptACK` (virtio 1.1 §4.2.2).
    fn ack_interrupt(&mut self) {}
}

/// The kernel-mapped register windows that make up a modern virtio PCI
/// device, plus the notify-offset multiplier the device advertised in
/// its notification capability.
///
/// This is the *construction seam* for a PCI [`Transport`]: the ring-0
/// provisioning walk in `kernel/virtio` maps each window through the
/// capability-checked MMIO-map facility and assembles this descriptor,
/// and the concrete `drivers/bus/virtio::PciTransport` is built from it.
/// It lives here, beside the [`Transport`] trait, so the kernel-side
/// walk can name the builder input without depending on the bus driver
/// crate (`kernel/* → lib/*`, never a driver).
#[derive(Debug)]
pub struct PciTransportWindows {
    /// Common-configuration structure window.
    pub common: RegisterWindow,
    /// Notification area window.
    pub notify: RegisterWindow,
    /// ISR-status window (one byte at offset 0).
    pub isr: RegisterWindow,
    /// Device-specific configuration window.
    pub device: RegisterWindow,
    /// `notify_off_multiplier` from the notification capability
    /// (virtio 1.1 §4.1.4.4). The notify address for a queue is
    /// `queue_notify_off * notify_off_multiplier`.
    pub notify_off_multiplier: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_helpers_round_trip() {
        let s = Status::default()
            .with(Status::ACKNOWLEDGE)
            .with(Status::DRIVER)
            .with(Status::FEATURES_OK);
        assert!(s.contains(Status::ACKNOWLEDGE));
        assert!(s.contains(Status::DRIVER));
        assert!(s.contains(Status::FEATURES_OK));
        assert!(!s.contains(Status::DRIVER_OK));
        assert_eq!(s.bits(), 1 | 2 | 8);
        assert_eq!(Status::from_bits(0xFF).bits(), 0xFF);
    }

    #[test]
    fn the_register_halves_are_exact_inverses() {
        for value in [
            0,
            1,
            0xFFFF_FFFF,
            0x1_0000_0000,
            0x1234_5678_9ABC_DEF0,
            u64::MAX,
        ] {
            let (low, high) = le_halves(value);
            assert_eq!(u64_from_le_halves(low, high), value, "{value:#x}");
        }
        // The split is by position, not by magnitude: neither half borrows a
        // bit from the other.
        assert_eq!(le_halves(0x1234_5678_9ABC_DEF0), (0x9ABC_DEF0, 0x1234_5678));
    }

    #[test]
    fn a_reset_confirms_once_the_status_reads_zero() {
        let mut reads = [3u32, 1, 0, 7].into_iter();
        assert_eq!(await_reset_within(8, || reads.next()), Ok(()));
        assert_eq!(reads.next(), Some(7), "no read past the confirmation");
    }

    #[test]
    fn a_device_that_never_clears_its_status_fails_the_reset() {
        let mut reads = 0;
        let outcome = await_reset_within(8, || {
            reads += 1;
            Some(Status::DRIVER_OK.into())
        });
        assert_eq!(outcome, Err(VirtioError::DeviceFault));
        assert_eq!(reads, 8, "bounded by the budget");
    }

    #[test]
    fn an_unreadable_status_fails_the_reset_at_once() {
        let mut reads = 0;
        let outcome = await_reset_within(8, || {
            reads += 1;
            None
        });
        assert_eq!(outcome, Err(VirtioError::DeviceFault));
        assert_eq!(reads, 1);
    }

    #[test]
    fn virtio_error_maps_to_driver_error() {
        assert_eq!(
            VirtioError::FeaturesRejected.as_driver_error(),
            DriverError::DeviceFault
        );
        assert_eq!(
            VirtioError::QueueIndexOutOfRange.as_driver_error(),
            DriverError::OutOfRange
        );
        assert_eq!(VirtioError::QueueFull.as_driver_error(), DriverError::Busy);
    }
}
