//! Modern (virtio-1.x) PCI [`Transport`] over kernel-mapped register
//! windows.
//!
//! A modern virtio PCI device exposes its register blocks through a
//! set of PCI capabilities (virtio 1.1 §4.1.4): a *common
//! configuration* structure, a *notification* area, an *ISR status*
//! byte, and a *device-specific configuration* area. The bus driver
//! resolves each capability to a `(BAR, offset, length)` triple,
//! asks the kernel MMIO-map facility for a
//! [`RegisterWindow`](rustos_abi::RegisterWindow) over it, and hands
//! the four windows to [`PciTransport::new`].
//!
//! This type therefore performs **no** pointer arithmetic and holds
//! **no** ambient authority: it can only touch registers the kernel
//! chose to map for the owning driver task. Every
//! register access goes through the bounds-checked accessors on
//! [`RegisterWindow`](rustos_abi::RegisterWindow).
//!
//! # No panics on the production path
//!
//! The constructor validates that the common-configuration window is
//! at least [`common::CFG_LEN`] bytes long. Because every common-cfg
//! offset this module reads or writes is a compile-time constant
//! below that bound, the infallible [`Transport`] methods can treat
//! their accesses as in-bounds and fall back to a safe default on the
//! (then impossible) error rather than panicking.
//! The notify offset is device-supplied, so it is bounds-checked on
//! the fallible [`Transport::queue_set`] path before
//! [`Transport::notify`] ever uses it.

use alloc::vec;
use alloc::vec::Vec;

use rustos_virtio::{PciTransportWindows, Status, Transport, VirtioError};

/// The virtio "no vector" sentinel (virtio 1.1 §4.1.4.3): writing it
/// to `queue_msix_vector` / `msix_config` tells the device not to
/// raise an MSI-X interrupt for that source. Every vector register
/// reads back this value after a device reset.
pub const VIRTIO_MSI_NO_VECTOR: u16 = 0xFFFF;

/// Byte offsets within the virtio-1.x PCI *common configuration*
/// structure (virtio 1.1 §4.1.4.3, `struct virtio_pci_common_cfg`).
pub mod common {
    /// `device_feature_select` (`le32`).
    pub const DEVICE_FEATURE_SELECT: usize = 0x00;
    /// `device_feature` (`le32`, windowed by the select register).
    pub const DEVICE_FEATURE: usize = 0x04;
    /// `driver_feature_select` (`le32`).
    pub const DRIVER_FEATURE_SELECT: usize = 0x08;
    /// `driver_feature` (`le32`, windowed by the select register).
    pub const DRIVER_FEATURE: usize = 0x0C;
    /// `num_queues` (`le16`).
    pub const NUM_QUEUES: usize = 0x12;
    /// `device_status` (`u8`).
    pub const DEVICE_STATUS: usize = 0x14;
    /// `queue_select` (`le16`).
    pub const QUEUE_SELECT: usize = 0x16;
    /// `queue_size` (`le16`).
    pub const QUEUE_SIZE: usize = 0x18;
    /// `queue_msix_vector` (`le16`) — the MSI-X table entry the
    /// device signals when the selected queue's used ring advances
    /// (virtio 1.1 §4.1.4.3). Defaults to
    /// [`VIRTIO_MSI_NO_VECTOR`](super::VIRTIO_MSI_NO_VECTOR) on reset,
    /// which suppresses queue interrupts entirely.
    pub const QUEUE_MSIX_VECTOR: usize = 0x1A;
    /// `queue_enable` (`le16`).
    pub const QUEUE_ENABLE: usize = 0x1C;
    /// `queue_notify_off` (`le16`).
    pub const QUEUE_NOTIFY_OFF: usize = 0x1E;
    /// `queue_desc` (`le64`, written as two `le32` halves).
    pub const QUEUE_DESC: usize = 0x20;
    /// `queue_driver` (`le64`, the avail ring).
    pub const QUEUE_DRIVER: usize = 0x28;
    /// `queue_device` (`le64`, the used ring).
    pub const QUEUE_DEVICE: usize = 0x30;
    /// Minimum byte length a common-configuration window must have
    /// for every access in this module to be in bounds.
    pub const CFG_LEN: usize = 0x38;
}

/// Modern virtio-1.x PCI transport.
#[derive(Debug)]
pub struct PciTransport {
    windows: PciTransportWindows,
    num_queues: u16,
    selected_queue: u16,
    /// Per-queue notify byte offset into the notification window,
    /// recorded by [`Transport::queue_set`] and consumed by
    /// [`Transport::notify`]. `None` until the queue is programmed.
    notify_offsets: Vec<Option<u32>>,
    /// MSI-X table entry programmed into every queue's
    /// `queue_msix_vector` by [`Transport::queue_set`].
    /// [`VIRTIO_MSI_NO_VECTOR`] (the default) leaves queue interrupts
    /// suppressed; [`PciTransport::enable_msix`] selects a real entry.
    msix_vector: u16,
}

impl PciTransport {
    /// Build a transport from a device's four kernel-mapped register
    /// windows.
    ///
    /// Reads `num_queues` from the common-configuration window so the
    /// per-queue bookkeeping is sized to the device.
    ///
    /// # Errors
    ///
    /// * [`VirtioError::DeviceFault`] if the common-configuration
    ///   window is shorter than [`common::CFG_LEN`] (a malformed
    ///   capability), so every subsequent constant-offset access on
    ///   the infallible [`Transport`] methods is in bounds.
    pub fn new(windows: PciTransportWindows) -> Result<Self, VirtioError> {
        if windows.common.len() < common::CFG_LEN {
            return Err(VirtioError::DeviceFault);
        }
        let num_queues = windows
            .common
            .read_u16(common::NUM_QUEUES)
            .map_err(|_| VirtioError::DeviceFault)?;
        Ok(Self {
            windows,
            num_queues,
            selected_queue: 0,
            notify_offsets: vec![None; num_queues as usize],
            msix_vector: VIRTIO_MSI_NO_VECTOR,
        })
    }

    /// Select the MSI-X table entry the device signals on queue
    /// completion.
    ///
    /// Must be called **before** the queue is programmed (i.e. before
    /// [`Transport::queue_set`] runs, which the driver drives from
    /// `VirtioBlk::open`): [`Transport::queue_set`] copies this entry
    /// into the selected queue's `queue_msix_vector` register and
    /// validates the device accepted it. The matching PCI MSI-X table
    /// entry must already have been routed by the kernel
    /// ([`route_msix`](rustos_abi::driver::msix::MsixBus::route_msix)).
    ///
    /// Config-change interrupts are intentionally left disabled
    /// (`msix_config` stays [`VIRTIO_MSI_NO_VECTOR`]): a block device's
    /// configuration is static for the lifetime of this transport.
    pub fn enable_msix(&mut self, entry: u16) {
        self.msix_vector = entry;
    }

    /// Borrow the underlying windows (host-side test access; not part
    /// of the [`Transport`] surface).
    #[must_use]
    pub fn windows(&self) -> &PciTransportWindows {
        &self.windows
    }

    /// Read one half of the 64-bit feature bitmap selected by
    /// `select`. Returns 0 on the (constructor-excluded) bounds
    /// error.
    fn read_feature_half(&self, select: u32) -> u32 {
        let _ = self
            .windows
            .common
            .write_u32(common::DEVICE_FEATURE_SELECT, select);
        self.windows
            .common
            .read_u32(common::DEVICE_FEATURE)
            .unwrap_or(0)
    }

    /// Write one half of the 64-bit driver-feature bitmap selected by
    /// `select`.
    fn write_feature_half(&self, select: u32, value: u32) {
        let _ = self
            .windows
            .common
            .write_u32(common::DRIVER_FEATURE_SELECT, select);
        let _ = self.windows.common.write_u32(common::DRIVER_FEATURE, value);
    }

    /// Write a little-endian `u64` to the common-cfg register at
    /// `offset` as two `u32` halves (the window exposes no `u64`
    /// accessor).
    fn write_u64(&self, offset: usize, value: u64) -> Result<(), VirtioError> {
        #[allow(clippy::cast_possible_truncation)]
        let lo = value as u32;
        #[allow(clippy::cast_possible_truncation)]
        let hi = (value >> 32) as u32;
        self.windows
            .common
            .write_u32(offset, lo)
            .map_err(|_| VirtioError::DeviceFault)?;
        self.windows
            .common
            .write_u32(offset + 4, hi)
            .map_err(|_| VirtioError::DeviceFault)
    }
}

impl Transport for PciTransport {
    fn reset(&mut self) {
        // Writing 0 resets the device; virtio 1.1 §4.1.4.3 requires
        // the driver to re-read `device_status` until it reads 0
        // before re-initialising. A bounded loop keeps the wait
        // finite so a wedged device cannot hang the boot path.
        let _ = self.windows.common.write_u8(common::DEVICE_STATUS, 0);
        for _ in 0..1_000_000 {
            match self.windows.common.read_u8(common::DEVICE_STATUS) {
                Ok(0) => break,
                _ => core::hint::spin_loop(),
            }
        }
        self.selected_queue = 0;
        for slot in &mut self.notify_offsets {
            *slot = None;
        }
    }

    fn status(&self) -> Status {
        let bits = self
            .windows
            .common
            .read_u8(common::DEVICE_STATUS)
            .unwrap_or(0);
        Status::from_bits(bits)
    }

    fn set_status(&mut self, status: Status) {
        let _ = self
            .windows
            .common
            .write_u8(common::DEVICE_STATUS, status.bits());
    }

    fn device_features(&self) -> u64 {
        let lo = u64::from(self.read_feature_half(0));
        let hi = u64::from(self.read_feature_half(1));
        (hi << 32) | lo
    }

    fn set_driver_features(&mut self, features: u64) {
        #[allow(clippy::cast_possible_truncation)]
        let lo = features as u32;
        #[allow(clippy::cast_possible_truncation)]
        let hi = (features >> 32) as u32;
        self.write_feature_half(0, lo);
        self.write_feature_half(1, hi);
    }

    fn num_queues(&self) -> u16 {
        self.num_queues
    }

    fn queue_select(&mut self, queue: u16) -> Result<(), VirtioError> {
        if queue >= self.num_queues {
            return Err(VirtioError::QueueIndexOutOfRange);
        }
        self.windows
            .common
            .write_u16(common::QUEUE_SELECT, queue)
            .map_err(|_| VirtioError::DeviceFault)?;
        self.selected_queue = queue;
        Ok(())
    }

    fn queue_max_size(&self) -> u16 {
        self.windows
            .common
            .read_u16(common::QUEUE_SIZE)
            .unwrap_or(0)
    }

    fn queue_set(
        &mut self,
        size: u16,
        desc: u64,
        avail: u64,
        used: u64,
    ) -> Result<(), VirtioError> {
        let max = self.queue_max_size();
        if size == 0 || size > max {
            return Err(VirtioError::QueueSizeTooLarge);
        }
        self.windows
            .common
            .write_u16(common::QUEUE_SIZE, size)
            .map_err(|_| VirtioError::DeviceFault)?;
        self.write_u64(common::QUEUE_DESC, desc)?;
        self.write_u64(common::QUEUE_DRIVER, avail)?;
        self.write_u64(common::QUEUE_DEVICE, used)?;
        // Program the queue's MSI-X vector before enabling it. A device
        // that cannot honour the request reflects `VIRTIO_MSI_NO_VECTOR`
        // back on read (virtio 1.1 §4.1.4.3); fail closed so the driver
        // never parks on an interrupt the device will not raise.
        if self.msix_vector != VIRTIO_MSI_NO_VECTOR {
            self.windows
                .common
                .write_u16(common::QUEUE_MSIX_VECTOR, self.msix_vector)
                .map_err(|_| VirtioError::DeviceFault)?;
            let echoed = self
                .windows
                .common
                .read_u16(common::QUEUE_MSIX_VECTOR)
                .map_err(|_| VirtioError::DeviceFault)?;
            if echoed != self.msix_vector {
                return Err(VirtioError::DeviceFault);
            }
        }
        // Resolve and validate the notify address for this queue
        // before it is ever used by the infallible `notify`.
        let notify_off = self
            .windows
            .common
            .read_u16(common::QUEUE_NOTIFY_OFF)
            .map_err(|_| VirtioError::DeviceFault)?;
        let byte_off = u32::from(notify_off)
            .checked_mul(self.windows.notify_off_multiplier)
            .ok_or(VirtioError::DeviceFault)?;
        let end = (byte_off as usize)
            .checked_add(2)
            .ok_or(VirtioError::DeviceFault)?;
        if end > self.windows.notify.len() {
            return Err(VirtioError::DeviceFault);
        }
        self.notify_offsets[self.selected_queue as usize] = Some(byte_off);
        self.windows
            .common
            .write_u16(common::QUEUE_ENABLE, 1)
            .map_err(|_| VirtioError::DeviceFault)
    }

    fn notify(&mut self, queue: u16) {
        // The offset was resolved and bounds-checked in `queue_set`;
        // an unprogrammed or out-of-range queue is a driver bug, but
        // we still fail closed (skip the write) rather than panic.
        if let Some(Some(byte_off)) = self.notify_offsets.get(queue as usize) {
            let _ = self.windows.notify.write_u16(*byte_off as usize, queue);
        }
    }

    fn read_config(&self, offset: usize, buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = self.windows.device.read_u8(offset + i).unwrap_or(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use core::ptr::NonNull;
    use rustos_abi::RegisterWindow;
    use rustos_virtio::{MockHost, SplitQueue};

    /// One device register region. The leaked, 8-byte-aligned
    /// backing storage outlives every window built over it (the test
    /// process never frees it), and a region is accessed only through
    /// the volatile [`RegisterWindow`] accessors, so the driver-side
    /// window held by [`PciTransport`] and the test's `dev` window
    /// alias the same bytes exactly as real hardware does.
    struct Region {
        base: NonNull<u8>,
        len: usize,
    }

    impl Region {
        fn new(len: usize) -> Self {
            let words = len.div_ceil(8);
            let boxed = alloc::vec![0u64; words.max(1)].into_boxed_slice();
            let raw = Box::leak(boxed);
            let base = NonNull::new(raw.as_mut_ptr().cast::<u8>()).expect("non-null");
            Self { base, len }
        }

        /// Build a fresh window over the region. `phys` is synthetic.
        fn window(&self, phys: u64) -> RegisterWindow {
            // SAFETY: `base` covers `len` bytes of leaked storage that
            // lives for the rest of the process; the window only ever
            // performs volatile accesses, so aliasing windows are
            // sound for the single-threaded test.
            unsafe { RegisterWindow::from_mapping(phys, self.base, self.len) }
        }
    }

    /// A buffer-backed stand-in for a modern virtio PCI device. The
    /// `dev_*` windows let a test pre-load device-supplied registers
    /// (`num_queues`, `queue_size`, notify offset, device config) and
    /// read back whatever the driver programmed.
    struct FakeDevice {
        common: Region,
        notify: Region,
        isr: Region,
        device: Region,
        notify_off_multiplier: u32,
    }

    impl FakeDevice {
        fn new(notify_len: usize, device_len: usize, notify_off_multiplier: u32) -> Self {
            Self {
                common: Region::new(common::CFG_LEN),
                notify: Region::new(notify_len),
                isr: Region::new(4),
                device: Region::new(device_len),
                notify_off_multiplier,
            }
        }

        fn dev_common(&self) -> RegisterWindow {
            self.common.window(0xC000_0000)
        }
        fn dev_notify(&self) -> RegisterWindow {
            self.notify.window(0xC001_0000)
        }
        fn dev_device(&self) -> RegisterWindow {
            self.device.window(0xC002_0000)
        }

        fn windows(&self) -> PciTransportWindows {
            PciTransportWindows {
                common: self.common.window(0xC000_0000),
                notify: self.notify.window(0xC001_0000),
                isr: self.isr.window(0xC003_0000),
                device: self.device.window(0xC002_0000),
                notify_off_multiplier: self.notify_off_multiplier,
            }
        }

        fn transport(&self) -> PciTransport {
            PciTransport::new(self.windows()).expect("valid windows")
        }
    }

    #[test]
    fn new_rejects_short_common_window() {
        let short = Region::new(common::CFG_LEN - 1);
        let windows = PciTransportWindows {
            common: short.window(0),
            notify: Region::new(8).window(0),
            isr: Region::new(4).window(0),
            device: Region::new(8).window(0),
            notify_off_multiplier: 0,
        };
        assert!(matches!(
            PciTransport::new(windows),
            Err(VirtioError::DeviceFault)
        ));
    }

    #[test]
    fn new_reads_num_queues() {
        let dev = FakeDevice::new(8, 8, 4);
        dev.dev_common().write_u16(common::NUM_QUEUES, 3).unwrap();
        let t = dev.transport();
        assert_eq!(t.num_queues(), 3);
    }

    #[test]
    fn status_writes_reads_and_reset_clears() {
        let dev = FakeDevice::new(8, 8, 4);
        let mut t = dev.transport();
        let s = Status::default()
            .with(Status::ACKNOWLEDGE)
            .with(Status::DRIVER);
        t.set_status(s);
        assert_eq!(t.status().bits(), Status::ACKNOWLEDGE | Status::DRIVER);
        t.reset();
        assert_eq!(t.status().bits(), 0);
    }

    #[test]
    fn set_driver_features_writes_both_halves() {
        let dev = FakeDevice::new(8, 8, 4);
        let mut t = dev.transport();
        t.set_driver_features(0x0000_0001_0000_0002);
        // The fake does not window the feature register, so after the
        // two-step write the select register holds the high index and
        // the feature register holds the high half.
        let c = dev.dev_common();
        assert_eq!(c.read_u32(common::DRIVER_FEATURE_SELECT).unwrap(), 1);
        assert_eq!(c.read_u32(common::DRIVER_FEATURE).unwrap(), 1);
    }

    #[test]
    fn queue_select_rejects_out_of_range() {
        let dev = FakeDevice::new(8, 8, 4);
        dev.dev_common().write_u16(common::NUM_QUEUES, 1).unwrap();
        let mut t = dev.transport();
        assert_eq!(t.queue_select(0), Ok(()));
        assert_eq!(t.queue_select(1), Err(VirtioError::QueueIndexOutOfRange));
    }

    #[test]
    fn queue_set_programs_registers_and_records_notify() {
        let dev = FakeDevice::new(64, 8, 4);
        let c = dev.dev_common();
        c.write_u16(common::NUM_QUEUES, 1).unwrap();
        c.write_u16(common::QUEUE_SIZE, 8).unwrap(); // device max
        c.write_u16(common::QUEUE_NOTIFY_OFF, 2).unwrap();
        let mut t = dev.transport();
        t.queue_select(0).unwrap();
        assert_eq!(t.queue_max_size(), 8);
        t.queue_set(8, 0x1234_5678_9ABC_DEF0, 0x0011_2233, 0x4455_6677)
            .unwrap();
        // Driver-programmed registers are visible to the device.
        assert_eq!(c.read_u16(common::QUEUE_SIZE).unwrap(), 8);
        assert_eq!(c.read_u32(common::QUEUE_DESC).unwrap(), 0x9ABC_DEF0);
        assert_eq!(c.read_u32(common::QUEUE_DESC + 4).unwrap(), 0x1234_5678);
        assert_eq!(c.read_u32(common::QUEUE_DRIVER).unwrap(), 0x0011_2233);
        assert_eq!(c.read_u32(common::QUEUE_DEVICE).unwrap(), 0x4455_6677);
        assert_eq!(c.read_u16(common::QUEUE_ENABLE).unwrap(), 1);
        // notify(0) writes the queue index to off * multiplier = 8.
        t.notify(0);
        assert_eq!(dev.dev_notify().read_u16(2 * 4).unwrap(), 0);
    }

    #[test]
    fn queue_set_skips_msix_vector_by_default() {
        // Without `enable_msix`, the transport leaves `queue_msix_vector`
        // at the device's reset default (`VIRTIO_MSI_NO_VECTOR`).
        let dev = FakeDevice::new(64, 8, 4);
        let c = dev.dev_common();
        c.write_u16(common::NUM_QUEUES, 1).unwrap();
        c.write_u16(common::QUEUE_SIZE, 8).unwrap();
        c.write_u16(common::QUEUE_MSIX_VECTOR, VIRTIO_MSI_NO_VECTOR)
            .unwrap();
        let mut t = dev.transport();
        t.queue_select(0).unwrap();
        t.queue_set(8, 1, 2, 3).unwrap();
        assert_eq!(
            c.read_u16(common::QUEUE_MSIX_VECTOR).unwrap(),
            VIRTIO_MSI_NO_VECTOR
        );
    }

    #[test]
    fn queue_set_programs_enabled_msix_vector() {
        // `enable_msix(entry)` programs the queue's `queue_msix_vector`
        // and validates the device echoed the entry back.
        let dev = FakeDevice::new(64, 8, 4);
        let c = dev.dev_common();
        c.write_u16(common::NUM_QUEUES, 1).unwrap();
        c.write_u16(common::QUEUE_SIZE, 8).unwrap();
        let mut t = dev.transport();
        t.enable_msix(0);
        t.queue_select(0).unwrap();
        t.queue_set(8, 1, 2, 3).unwrap();
        assert_eq!(c.read_u16(common::QUEUE_MSIX_VECTOR).unwrap(), 0);
        assert_eq!(c.read_u16(common::QUEUE_ENABLE).unwrap(), 1);
    }

    #[test]
    fn queue_set_rejects_oversize() {
        let dev = FakeDevice::new(64, 8, 4);
        let c = dev.dev_common();
        c.write_u16(common::NUM_QUEUES, 1).unwrap();
        c.write_u16(common::QUEUE_SIZE, 8).unwrap();
        let mut t = dev.transport();
        t.queue_select(0).unwrap();
        assert_eq!(
            t.queue_set(16, 1, 2, 3),
            Err(VirtioError::QueueSizeTooLarge)
        );
        assert_eq!(t.queue_set(0, 1, 2, 3), Err(VirtioError::QueueSizeTooLarge));
    }

    #[test]
    fn queue_set_rejects_notify_offset_out_of_bounds() {
        // notify window only 8 bytes; off 4 * multiplier 4 = 16 → OOB.
        let dev = FakeDevice::new(8, 8, 4);
        let c = dev.dev_common();
        c.write_u16(common::NUM_QUEUES, 1).unwrap();
        c.write_u16(common::QUEUE_SIZE, 8).unwrap();
        c.write_u16(common::QUEUE_NOTIFY_OFF, 4).unwrap();
        let mut t = dev.transport();
        t.queue_select(0).unwrap();
        assert_eq!(t.queue_set(8, 1, 2, 3), Err(VirtioError::DeviceFault));
    }

    #[test]
    fn notify_unprogrammed_queue_is_a_noop() {
        let dev = FakeDevice::new(8, 8, 4);
        dev.dev_common().write_u16(common::NUM_QUEUES, 1).unwrap();
        let mut t = dev.transport();
        // No queue_set: notify must not write or panic.
        t.notify(0);
        t.notify(5); // out of range index — still a no-op.
        assert_eq!(dev.dev_notify().read_u16(0).unwrap(), 0);
    }

    #[test]
    fn read_config_reads_device_window_and_zero_fills_overflow() {
        let dev = FakeDevice::new(8, 8, 0);
        let d = dev.dev_device();
        d.write_u32(0, 0x0403_0201).unwrap();
        d.write_u32(4, 0x0807_0605).unwrap();
        let t = dev.transport();
        let mut buf = [0u8; 4];
        t.read_config(2, &mut buf);
        assert_eq!(buf, [3, 4, 5, 6]);
        // Reading past the 8-byte window zero-fills.
        let mut over = [0xCDu8; 4];
        t.read_config(6, &mut over);
        assert_eq!(over, [7, 8, 0, 0]);
    }

    #[test]
    fn split_queue_drives_pci_transport() {
        // Prove the transport integrates with the generic split-queue
        // setup path: SplitQueue::new selects, sizes, and programs the
        // queue through the PciTransport.
        let dev = FakeDevice::new(64, 8, 4);
        let c = dev.dev_common();
        c.write_u16(common::NUM_QUEUES, 1).unwrap();
        c.write_u16(common::QUEUE_SIZE, 8).unwrap();
        c.write_u16(common::QUEUE_NOTIFY_OFF, 1).unwrap();
        let mut t = dev.transport();
        let host: &'static MockHost = Box::leak(Box::new(MockHost::new()));
        let q = SplitQueue::new(&mut t, host, 0, 8).expect("queue setup");
        assert_eq!(q.size(), 8);
        // The queue allocated its descriptor table through the host
        // and programmed its phys into the device's QUEUE_DESC
        // register: a non-zero address must now be visible there, and
        // the queue must have been enabled.
        let lo = c.read_u32(common::QUEUE_DESC).unwrap();
        let hi = c.read_u32(common::QUEUE_DESC + 4).unwrap();
        assert_ne!((u64::from(hi) << 32) | u64::from(lo), 0);
        assert_eq!(c.read_u16(common::QUEUE_ENABLE).unwrap(), 1);
    }
}
