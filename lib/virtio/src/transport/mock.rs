//! [`MockTransport`], the in-process software peer that plays the virtio
//! device in the unit tests of this crate and of every virtio driver, so the
//! queue protocol runs without real hardware. Built only for tests, behind the
//! crate's `mock` feature.
//!
//! It reads the descriptor / avail / used memory the driver published through
//! `crate::queue::ring_view::RingView`, drains chains as the device would, and
//! answers each through the [`DeviceShim`] the test installs for its queue.

use super::{Status, Transport, VirtioError};
use crate::queue::ring_view::RingView;
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

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
    /// Resets still to confirm before every later one is refused; `None`
    /// confirms them all. See [`Self::refuse_resets_after`].
    resets_confirmed_left: Option<u32>,
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
            resets_confirmed_left: None,
        }
    }

    /// Model a device that wedges after `confirmed` more resets: every later
    /// reset is refused and leaves the device's state, and any memory it was
    /// given, in its hands.
    pub fn refuse_resets_after(&mut self, confirmed: u32) {
        self.resets_confirmed_left = Some(confirmed);
    }

    /// Advertise at most `max` descriptors for `queue` alone, as a device
    /// whose queues differ in depth does.
    pub fn set_queue_max(&mut self, queue: u16, max: u16) {
        self.queues[usize::from(queue)].max_size = max;
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
    /// (CWE-1257 / Thunderclap). The fuzz harness uses it to show the
    /// driver's free list and chain links, which live in its own memory,
    /// cannot be corrupted through the table. A table scribbled over this way
    /// is never drained: [`Self::drain_queue`] builds slices from the
    /// descriptors' addresses.
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

    /// The descriptor indices of the chain the driver published under `head`
    /// on `queue`, in order, as a device walking the table reaches them.
    ///
    /// # Errors
    ///
    /// * [`VirtioError::QueueIndexOutOfRange`] if `queue` is unknown.
    /// * [`VirtioError::DeviceFault`] if the queue has not been programmed.
    /// * [`VirtioError::DescriptorTableOverflow`] if the chain leaves the
    ///   table or loops.
    pub fn chain_descriptors(&self, queue: u16, head: u16) -> Result<Vec<u16>, VirtioError> {
        let q = self
            .queues
            .get(usize::from(queue))
            .ok_or(VirtioError::QueueIndexOutOfRange)?;
        if q.size == 0 || q.desc_phys == 0 {
            return Err(VirtioError::DeviceFault);
        }
        RingView::from_phys(q.size, q.desc_phys, q.avail_phys, q.used_phys).chain_indices(head)
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
        // identity-mapped pointers to driver-owned storage; the mock peer
        // reaches them only through `RingView`, which bounds every descriptor
        // index it reads by `q.size` and every chain by the table's length.
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
    fn reset(&mut self) -> Result<(), VirtioError> {
        if let Some(left) = self.resets_confirmed_left {
            if left == 0 {
                return Err(VirtioError::DeviceFault);
            }
            self.resets_confirmed_left = Some(left - 1);
        }
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
        Ok(())
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

impl MockTransport {
    /// Share the mock between the driver under test, which owns one handle as
    /// its transport, and the [`MockHost`](crate::MockHost) that plays its
    /// device through another.
    #[must_use]
    pub fn into_shared(self) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(self))
    }
}

/// A shared mock is borrowed afresh for each call, so the host playing the
/// device can drain it between them.
impl Transport for Rc<RefCell<MockTransport>> {
    fn reset(&mut self) -> Result<(), VirtioError> {
        self.borrow_mut().reset()
    }
    fn status(&self) -> Status {
        self.borrow().status()
    }
    fn set_status(&mut self, status: Status) {
        self.borrow_mut().set_status(status);
    }
    fn device_features(&self) -> u64 {
        self.borrow().device_features()
    }
    fn set_driver_features(&mut self, features: u64) {
        self.borrow_mut().set_driver_features(features);
    }
    fn num_queues(&self) -> u16 {
        self.borrow().num_queues()
    }
    fn queue_select(&mut self, queue: u16) -> Result<(), VirtioError> {
        self.borrow_mut().queue_select(queue)
    }
    fn queue_max_size(&self) -> u16 {
        self.borrow().queue_max_size()
    }
    fn queue_set(
        &mut self,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    ) -> Result<(), VirtioError> {
        self.borrow_mut().queue_set(size, desc, avail, used)
    }
    fn notify(&mut self, queue: u16) {
        self.borrow_mut().notify(queue);
    }
    fn read_config(&self, offset: usize, buf: &mut [u8]) {
        self.borrow().read_config(offset, buf);
    }
    fn ack_interrupt(&mut self) {
        self.borrow_mut().ack_interrupt();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mock_wedges_after_its_confirmed_resets_and_keeps_its_state() {
        let mut t = MockTransport::new(1, 8, 0, 0);
        t.refuse_resets_after(1);
        assert_eq!(t.reset(), Ok(()));
        t.set_status(Status::default().with(Status::DRIVER_OK));
        assert_eq!(t.reset(), Err(VirtioError::DeviceFault));
        assert_eq!(t.reset(), Err(VirtioError::DeviceFault), "wedged for good");
        assert!(t.status().contains(Status::DRIVER_OK));
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
