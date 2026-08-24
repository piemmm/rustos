//! [`Transport`] trait + in-process [`MockTransport`].
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
//! [`MockTransport`] is the in-process software peer that the unit
//! tests in this crate (and in `virtio_blk` / `virtio_net`) use to
//! exercise the queue protocol without real hardware. It owns a
//! `crate::queue::ring_view::RingView` over the same descriptor
//! / avail / used memory the driver published, drains chains as the
//! "device" would, and stores per-queue per-device behaviour
//! through a [`DeviceShim`] callback the driver installs.

use crate::queue::ring_view::RingView;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_abi::{DriverError, RegisterWindow};

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
    /// Reset the device and re-establish a clean status byte.
    fn reset(&mut self);
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
    /// [`MockTransport`] (no real device). The modern **MMIO** transport
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

/// Callback the test peer invokes for each chain the driver
/// publishes on a queue.
///
/// The callback inspects the chain's read-only / write-only
/// descriptor halves (via [`ChainView`]) and returns the number of
/// bytes the device "wrote" into the write-only descriptors.
/// Returning `Err(VirtioError::DeviceFault)` surfaces a device
/// error to the driver through the used ring (length = 0 + a peer
/// status byte the caller can stash in its own descriptor).
pub type DeviceShim = Box<dyn FnMut(&mut ChainView<'_>) -> Result<u32, VirtioError>>;

/// View the [`DeviceShim`] gets over a published chain.
pub struct ChainView<'a> {
    /// Read-only descriptor segments (driver → device).
    pub device_read: Vec<&'a [u8]>,
    /// Write-only descriptor segments (device → driver). The shim
    /// writes its response bytes into these slices.
    pub device_write: Vec<&'a mut [u8]>,
}

/// Per-queue mock state managed by [`MockTransport`].
struct MockQueue {
    size: u16,
    max_size: u16,
    desc_phys: u64,
    avail_phys: u64,
    used_phys: u64,
    last_seen_avail_idx: u16,
    /// Packed-ring device cursor: next ring position the mock device
    /// will inspect, and its Device Ring Wrap Counter (virtio 1.1
    /// §2.7.1). Unused by the split drain path.
    packed_dev_idx: u16,
    packed_dev_wrap: bool,
    shim: Option<DeviceShim>,
}

impl MockQueue {
    fn new(max_size: u16) -> Self {
        Self {
            size: 0,
            max_size,
            desc_phys: 0,
            avail_phys: 0,
            used_phys: 0,
            last_seen_avail_idx: 0,
            packed_dev_idx: 0,
            packed_dev_wrap: true,
            shim: None,
        }
    }
}

/// In-process software peer that pretends to be the virtio device.
///
/// `MockTransport` records every register write and, on each
/// [`Transport::notify`] call, drains the avail ring of the
/// selected queue through the [`DeviceShim`] the test installs.
/// Crucially the mock's `phys` address space is the test process's
/// own — a `phys` is just a cast pointer — which is what lets
/// [`crate::queue::SplitQueue::poll_used`] read back the response
/// bytes the shim wrote.
pub struct MockTransport {
    device_features: u64,
    driver_features: u64,
    status: Status,
    selected_queue: u16,
    queues: Vec<MockQueue>,
    config: Vec<u8>,
    /// Records of every notify-call (in order), for assertions in
    /// unit tests.
    pub notify_log: RefCell<Vec<u16>>,
    /// Number of [`Transport::ack_interrupt`] calls, for assertions that
    /// a driver acknowledges the device once per wait + drain cycle.
    pub ack_interrupts: u32,
    /// When set, [`Transport::notify`] drains the notified queue inline
    /// (QEMU-accurate synchronous notify); see [`Self::set_synchronous_notify`].
    synchronous_notify: bool,
}

impl MockTransport {
    /// Build a `MockTransport` with `num_queues` queues each capped
    /// at `queue_max_size`, the given device-features bitmap, and a
    /// `config_len`-byte device-config window.
    #[must_use]
    pub fn new(
        num_queues: u16,
        queue_max_size: u16,
        device_features: u64,
        config_len: usize,
    ) -> Self {
        let mut queues = Vec::with_capacity(num_queues as usize);
        for _ in 0..num_queues {
            queues.push(MockQueue::new(queue_max_size));
        }
        Self {
            device_features,
            driver_features: 0,
            status: Status::default(),
            selected_queue: 0,
            queues,
            config: alloc::vec![0u8; config_len],
            notify_log: RefCell::new(Vec::new()),
            ack_interrupts: 0,
            synchronous_notify: false,
        }
    }

    /// Make [`Transport::notify`] process the notified queue synchronously
    /// (drain its shim on the notifying call), modelling QEMU/real
    /// hardware where a notify vmexit processes the queue inline. Off by
    /// default so most tests keep explicit control of when the device
    /// runs; a driver that polls for a completion inline (the multiqueue
    /// control-queue handshake) turns it on.
    pub fn set_synchronous_notify(&mut self, on: bool) {
        self.synchronous_notify = on;
    }

    /// Overwrite the device-configuration window. Used by `virtio_blk`
    /// / `virtio_net` unit tests to plant geometry / MAC bytes.
    pub fn set_config(&mut self, offset: usize, bytes: &[u8]) {
        self.config[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    /// Install a [`DeviceShim`] for `queue`. Existing shims are
    /// replaced.
    pub fn install_shim(&mut self, queue: u16, shim: DeviceShim) {
        self.queues[queue as usize].shim = Some(shim);
    }

    /// Test/fuzz-only **hostile-device** seam: publish a used-ring
    /// completion for `queue` naming descriptor head `head` and reporting
    /// `written` bytes, then advance the device's `used.idx`.
    ///
    /// Unlike [`Self::drain_queue`], which only ever publishes a head it
    /// genuinely collected from the avail ring, this plants an *arbitrary*
    /// `head` — including one outside the granted descriptor table. That
    /// is exactly the corruption a buggy or hostile device can write
    /// (CWE-1257 / Thunderclap), and it is what the
    /// fuzz harness drives at [`crate::queue::SplitQueue::poll_used`].
    ///
    /// # Errors
    ///
    /// * [`VirtioError::QueueIndexOutOfRange`] if `queue` is unknown.
    /// * [`VirtioError::DeviceFault`] if the queue has not been programmed.
    pub fn publish_raw_used(
        &mut self,
        queue: u16,
        head: u16,
        written: u32,
    ) -> Result<(), VirtioError> {
        let q = self
            .queues
            .get(queue as usize)
            .ok_or(VirtioError::QueueIndexOutOfRange)?;
        if q.size == 0 || q.used_phys == 0 {
            return Err(VirtioError::DeviceFault);
        }
        let view = RingView::from_phys(q.size, q.desc_phys, q.avail_phys, q.used_phys);
        view.publish_used(head, written);
        Ok(())
    }

    /// Test/fuzz-only **hostile-device** seam: overwrite one byte of
    /// `queue`'s descriptor table at `byte_offset`, modelling a device DMA
    /// write that scribbles a descriptor field — e.g. a chain `next` link
    /// (CWE-1257 / Thunderclap). The fuzz harness
    /// uses it to make `poll_used`'s reclaim walk attempt to leave the
    /// granted region, asserting the driver bails instead.
    ///
    /// `byte_offset` must lie inside the descriptor table
    /// (`< desc_table_size(size)`); an out-of-range offset is a no-op, so
    /// the harness itself never writes outside driver-owned storage.
    ///
    /// # Errors
    ///
    /// * [`VirtioError::QueueIndexOutOfRange`] if `queue` is unknown.
    /// * [`VirtioError::DeviceFault`] if the queue has not been programmed.
    pub fn poke_descriptor(
        &mut self,
        queue: u16,
        byte_offset: usize,
        value: u8,
    ) -> Result<(), VirtioError> {
        let q = self
            .queues
            .get(queue as usize)
            .ok_or(VirtioError::QueueIndexOutOfRange)?;
        if q.size == 0 || q.desc_phys == 0 {
            return Err(VirtioError::DeviceFault);
        }
        if byte_offset >= crate::queue::SplitQueue::desc_table_size(q.size) {
            return Ok(());
        }
        // SAFETY: `desc_phys` is the identity-mapped pointer to the
        // driver-owned descriptor table; `byte_offset` was bounded to
        // `< desc_table_size(size)` above, so the write stays inside that
        // table. The mock peer is the only other holder and we have
        // `&mut self`. This is a mock-peer-only adversarial seam.
        unsafe {
            (q.desc_phys as *mut u8).add(byte_offset).write(value);
        }
        Ok(())
    }

    /// The driver-features bitmap the driver wrote during
    /// negotiation. Used by tests to assert feature wiring.
    #[must_use]
    pub fn negotiated_driver_features(&self) -> u64 {
        self.driver_features
    }

    /// Drive the **packed** peer once: drain every newly-available
    /// packed descriptor chain on `queue` through the shim, writing
    /// completions back in-band (virtio 1.1 §2.7).
    ///
    /// Returns the number of chains the peer drained.
    ///
    /// # Errors
    ///
    /// Propagates the shim's [`VirtioError`].
    pub fn drain_packed_queue(&mut self, queue: u16) -> Result<usize, VirtioError> {
        use crate::packed::packed_ring_view::PackedRingView;
        let idx = queue as usize;
        if idx >= self.queues.len() {
            return Err(VirtioError::QueueIndexOutOfRange);
        }
        let q = &mut self.queues[idx];
        if q.size == 0 || q.desc_phys == 0 {
            return Err(VirtioError::DeviceFault);
        }
        // SAFETY-INVARIANT: as in `drain_queue`, the descriptor-ring
        // phys the driver programmed is an identity-mapped pointer to
        // driver-owned storage; `PackedRingView` validates chain
        // lengths against `q.size`.
        let view = PackedRingView::from_phys(q.size, q.desc_phys);
        let mut drained = 0usize;
        loop {
            if !view.is_available(q.packed_dev_idx, q.packed_dev_wrap) {
                break;
            }
            let head = q.packed_dev_idx;
            let head_wrap = q.packed_dev_wrap;
            let collected = view.collect_chain(head, head_wrap)?;
            let mut chain = collected.chain;
            let shim = q.shim.as_mut().ok_or(VirtioError::DeviceFault)?;
            let written = shim(&mut chain)?;
            view.publish_used(head, head_wrap, collected.buffer_id, written);
            for _ in 0..collected.len {
                if q.packed_dev_idx + 1 == q.size {
                    q.packed_dev_idx = 0;
                    q.packed_dev_wrap = !q.packed_dev_wrap;
                } else {
                    q.packed_dev_idx += 1;
                }
            }
            drained += 1;
        }
        Ok(drained)
    }

    /// Drive the peer once: drain every new avail-ring entry on
    /// `queue` through the shim, populating the used ring.
    ///
    /// Returns the number of chains the peer drained.
    ///
    /// # Errors
    ///
    /// Propagates the shim's [`VirtioError`].
    pub fn drain_queue(&mut self, queue: u16) -> Result<usize, VirtioError> {
        let idx = queue as usize;
        if idx >= self.queues.len() {
            return Err(VirtioError::QueueIndexOutOfRange);
        }
        let q = &mut self.queues[idx];
        if q.size == 0 || q.desc_phys == 0 {
            return Err(VirtioError::DeviceFault);
        }
        // SAFETY-INVARIANT: phys addresses planted by the driver are
        // identity-mapped pointers to driver-owned storage; the mock
        // peer only inspects the bytes through `RingView`, whose
        // accessors validate `next` chain indices against `q.size`
        // and reject overflow.
        let view = RingView::from_phys(q.size, q.desc_phys, q.avail_phys, q.used_phys);
        let mut drained = 0usize;
        loop {
            let avail_idx = view.read_avail_idx();
            if q.last_seen_avail_idx == avail_idx {
                break;
            }
            let slot = q.last_seen_avail_idx % q.size;
            let head = view.read_avail_ring(slot);
            let mut chain = view.collect_chain(head)?;
            let shim = q.shim.as_mut().ok_or(VirtioError::DeviceFault)?;
            let written = shim(&mut chain)?;
            view.publish_used(head, written);
            drained += 1;
            q.last_seen_avail_idx = q.last_seen_avail_idx.wrapping_add(1);
        }
        Ok(drained)
    }
}

impl Transport for MockTransport {
    fn reset(&mut self) {
        self.status = Status::default();
        self.driver_features = 0;
        self.selected_queue = 0;
        for q in &mut self.queues {
            q.size = 0;
            q.desc_phys = 0;
            q.avail_phys = 0;
            q.used_phys = 0;
            q.last_seen_avail_idx = 0;
            q.packed_dev_idx = 0;
            q.packed_dev_wrap = true;
        }
    }
    fn status(&self) -> Status {
        self.status
    }
    fn set_status(&mut self, status: Status) {
        self.status = status;
    }
    fn device_features(&self) -> u64 {
        self.device_features
    }
    fn set_driver_features(&mut self, features: u64) {
        self.driver_features = features;
    }
    fn num_queues(&self) -> u16 {
        u16::try_from(self.queues.len()).unwrap_or(u16::MAX)
    }
    fn queue_select(&mut self, queue: u16) -> Result<(), VirtioError> {
        if (queue as usize) >= self.queues.len() {
            return Err(VirtioError::QueueIndexOutOfRange);
        }
        self.selected_queue = queue;
        Ok(())
    }
    fn queue_max_size(&self) -> u16 {
        self.queues[self.selected_queue as usize].max_size
    }
    fn queue_set(
        &mut self,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    ) -> Result<(), VirtioError> {
        let q = &mut self.queues[self.selected_queue as usize];
        if size > q.max_size || size == 0 {
            return Err(VirtioError::QueueSizeTooLarge);
        }
        q.size = size;
        q.desc_phys = desc;
        q.avail_phys = avail;
        q.used_phys = used;
        q.last_seen_avail_idx = 0;
        q.packed_dev_idx = 0;
        q.packed_dev_wrap = true;
        Ok(())
    }
    fn notify(&mut self, queue: u16) {
        self.notify_log.borrow_mut().push(queue);
        // By default the unit tests choose when to drain (so they can
        // assert intermediate state), so we do NOT auto-drain here. A test
        // that needs the QEMU-accurate *synchronous* notify (the device
        // processes the queue on the notifying vmexit) — e.g. the
        // multiqueue control-queue handshake, which the driver polls for
        // inline rather than waiting on the host — opts in through
        // [`Self::set_synchronous_notify`].
        if self.synchronous_notify {
            let _ = self.drain_queue(queue);
        }
    }
    fn read_config(&self, offset: usize, buf: &mut [u8]) {
        let end = offset + buf.len();
        if end <= self.config.len() {
            buf.copy_from_slice(&self.config[offset..end]);
        } else {
            // Reading past the end is a spec violation; fail closed
            // by leaving `buf` zeroed (caller has zero-init buffers).
            for b in buf.iter_mut() {
                *b = 0;
            }
        }
    }
    fn ack_interrupt(&mut self) {
        // No device line to de-assert; count the call so unit tests can
        // assert the driver acknowledged once per wait + drain cycle.
        self.ack_interrupts += 1;
    }
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

    #[test]
    fn mock_transport_records_register_writes() {
        let mut t = MockTransport::new(2, 8, 0x0000_00FF, 16);
        assert_eq!(t.num_queues(), 2);
        assert_eq!(t.device_features(), 0xFF);
        t.set_status(Status::default().with(Status::ACKNOWLEDGE));
        t.set_driver_features(0x0F);
        assert!(t.status().contains(Status::ACKNOWLEDGE));
        assert_eq!(t.negotiated_driver_features(), 0x0F);
        assert!(t.queue_select(0).is_ok());
        assert_eq!(t.queue_max_size(), 8);
        // Out-of-range queue select.
        assert_eq!(t.queue_select(2), Err(VirtioError::QueueIndexOutOfRange));
    }

    #[test]
    fn mock_transport_rejects_oversize_queue() {
        let mut t = MockTransport::new(1, 8, 0, 0);
        t.queue_select(0).unwrap();
        assert_eq!(
            t.queue_set(16, 1, 2, 3),
            Err(VirtioError::QueueSizeTooLarge)
        );
        assert_eq!(t.queue_set(0, 1, 2, 3), Err(VirtioError::QueueSizeTooLarge));
    }

    #[test]
    fn read_config_returns_planted_bytes() {
        let mut t = MockTransport::new(1, 8, 0, 8);
        t.set_config(0, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let mut buf = [0u8; 4];
        t.read_config(2, &mut buf);
        assert_eq!(buf, [3, 4, 5, 6]);
        // Out-of-range read leaves buf untouched (zeroed by caller).
        let mut overflow = [0xCDu8; 4];
        t.read_config(8, &mut overflow);
        assert_eq!(overflow, [0u8; 4]);
    }
}
