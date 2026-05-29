//! Modern (virtio-1.x) MMIO [`Transport`] over a kernel-mapped
//! register window.
//!
//! A virtio-MMIO device (virtio 1.1 §4.2) exposes a single,
//! contiguous register block — the layout the QEMU `-M virt`
//! `virtio-mmio` transport and the RISC-V / `AArch64` device-tree
//! nodes advertise. The bus driver discovers the block's
//! `(base, length)` from the boot device tree, asks the kernel
//! MMIO-map facility for a [`RegisterWindow`] over it, and hands that
//! single window to [`MmioTransport::new`].
//!
//! Unlike the PCI transport (four capability-selected windows), the
//! MMIO transport reads and writes every register at a fixed offset
//! inside one window. It therefore performs **no** pointer arithmetic
//! and holds **no** ambient authority: it can only touch the register
//! block the kernel chose to map for the owning driver task
//! (`AGENTS.md` §4). Every access goes through the bounds-checked
//! accessors on [`RegisterWindow`].
//!
//! # No panics on the production path
//!
//! The constructor validates the magic value, a supported version,
//! and that the window spans the whole register block
//! ([`regs::WINDOW_MIN_LEN`]). Because every register this module
//! reads or writes lives at a compile-time-constant offset below that
//! bound, the infallible [`Transport`] methods treat their accesses as
//! in-bounds and fall back to a safe default on the (then impossible)
//! error rather than panicking (`AGENTS.md` §2.9).

use rustos_abi::RegisterWindow;

use crate::transport::{Status, Transport, VirtioError};

/// Byte offsets within the virtio-MMIO register block (virtio 1.1
/// §4.2.2, `MMIO Device Register Layout`). All registers are 32 bits
/// wide and naturally aligned.
pub mod regs {
    /// `MagicValue` (R) — must read `"virt"` (little-endian
    /// `0x7472_6976`).
    pub const MAGIC: usize = 0x000;
    /// `Version` (R) — `2` for a modern (non-legacy) device.
    pub const VERSION: usize = 0x004;
    /// `DeviceID` (R) — virtio device type; `0` means "no device".
    pub const DEVICE_ID: usize = 0x008;
    /// `DeviceFeatures` (R) — windowed by `DeviceFeaturesSel`.
    pub const DEVICE_FEATURES: usize = 0x010;
    /// `DeviceFeaturesSel` (W) — selects which 32-bit half
    /// `DeviceFeatures` exposes.
    pub const DEVICE_FEATURES_SEL: usize = 0x014;
    /// `DriverFeatures` (W) — windowed by `DriverFeaturesSel`.
    pub const DRIVER_FEATURES: usize = 0x020;
    /// `DriverFeaturesSel` (W) — selects which 32-bit half
    /// `DriverFeatures` writes.
    pub const DRIVER_FEATURES_SEL: usize = 0x024;
    /// `QueueSel` (W) — selects the queue the `Queue*` registers act
    /// on.
    pub const QUEUE_SEL: usize = 0x030;
    /// `QueueNumMax` (R) — maximum size of the selected queue.
    pub const QUEUE_NUM_MAX: usize = 0x034;
    /// `QueueNum` (W) — size the driver programs for the selected
    /// queue.
    pub const QUEUE_NUM: usize = 0x038;
    /// `QueueReady` (RW) — write `1` to make the selected queue live.
    pub const QUEUE_READY: usize = 0x044;
    /// `QueueNotify` (W) — write the queue index to notify the device.
    pub const QUEUE_NOTIFY: usize = 0x050;
    /// `Status` (RW) — the device-status byte (low 8 bits used).
    pub const STATUS: usize = 0x070;
    /// `QueueDescLow` (W) — low half of the descriptor-table address.
    pub const QUEUE_DESC_LOW: usize = 0x080;
    /// `QueueDescHigh` (W) — high half of the descriptor-table
    /// address.
    pub const QUEUE_DESC_HIGH: usize = 0x084;
    /// `QueueDriverLow` (W) — low half of the avail-ring address.
    pub const QUEUE_DRIVER_LOW: usize = 0x090;
    /// `QueueDriverHigh` (W) — high half of the avail-ring address.
    pub const QUEUE_DRIVER_HIGH: usize = 0x094;
    /// `QueueDeviceLow` (W) — low half of the used-ring address.
    pub const QUEUE_DEVICE_LOW: usize = 0x0A0;
    /// `QueueDeviceHigh` (W) — high half of the used-ring address.
    pub const QUEUE_DEVICE_HIGH: usize = 0x0A4;
    /// Device-configuration space begins here.
    pub const CONFIG: usize = 0x100;
    /// Minimum window length for every register access in this module
    /// (through the last queue-address register) to be in bounds.
    pub const WINDOW_MIN_LEN: usize = QUEUE_DEVICE_HIGH + 4;

    /// Little-endian encoding of `"virt"` expected in [`MAGIC`].
    pub const MAGIC_VALUE: u32 = 0x7472_6976;
    /// The only device version this transport drives (modern virtio).
    pub const VERSION_MODERN: u32 = 2;
}

/// Modern virtio-1.x MMIO transport over a single kernel-mapped
/// register window.
#[derive(Debug)]
pub struct MmioTransport {
    window: RegisterWindow,
    selected_queue: u16,
}

impl MmioTransport {
    /// Build a transport from a device's single kernel-mapped register
    /// window.
    ///
    /// Validates the device's identity registers so every subsequent
    /// constant-offset access on the infallible [`Transport`] methods
    /// is provably in bounds and addresses a real virtio-MMIO device.
    ///
    /// # Errors
    ///
    /// * [`VirtioError::DeviceFault`] if the window is shorter than
    ///   [`regs::WINDOW_MIN_LEN`], the magic value is not `"virt"`, the
    ///   reported version is not [`regs::VERSION_MODERN`], or the
    ///   device-id register reads `0` (an empty virtio-MMIO slot).
    pub fn new(window: RegisterWindow) -> Result<Self, VirtioError> {
        if window.len() < regs::WINDOW_MIN_LEN {
            return Err(VirtioError::DeviceFault);
        }
        let magic = window
            .read_u32(regs::MAGIC)
            .map_err(|_| VirtioError::DeviceFault)?;
        if magic != regs::MAGIC_VALUE {
            return Err(VirtioError::DeviceFault);
        }
        let version = window
            .read_u32(regs::VERSION)
            .map_err(|_| VirtioError::DeviceFault)?;
        if version != regs::VERSION_MODERN {
            return Err(VirtioError::DeviceFault);
        }
        let device_id = window
            .read_u32(regs::DEVICE_ID)
            .map_err(|_| VirtioError::DeviceFault)?;
        if device_id == 0 {
            return Err(VirtioError::DeviceFault);
        }
        Ok(Self {
            window,
            selected_queue: 0,
        })
    }

    /// Borrow the underlying window (host-side test access; not part
    /// of the [`Transport`] surface).
    #[must_use]
    pub fn window(&self) -> &RegisterWindow {
        &self.window
    }

    /// Read one half of the 64-bit device-feature bitmap selected by
    /// `select`. Returns 0 on the (constructor-excluded) bounds error.
    fn read_feature_half(&self, select: u32) -> u32 {
        let _ = self.window.write_u32(regs::DEVICE_FEATURES_SEL, select);
        self.window.read_u32(regs::DEVICE_FEATURES).unwrap_or(0)
    }

    /// Write one half of the 64-bit driver-feature bitmap selected by
    /// `select`.
    fn write_feature_half(&self, select: u32, value: u32) {
        let _ = self.window.write_u32(regs::DRIVER_FEATURES_SEL, select);
        let _ = self.window.write_u32(regs::DRIVER_FEATURES, value);
    }

    /// Write a little-endian `u64` to the `*Low`/`*High` register pair
    /// beginning at `low_offset` (the window exposes no `u64`
    /// accessor).
    fn write_u64_pair(&self, low_offset: usize, value: u64) -> Result<(), VirtioError> {
        #[allow(clippy::cast_possible_truncation)]
        let lo = value as u32;
        #[allow(clippy::cast_possible_truncation)]
        let hi = (value >> 32) as u32;
        self.window
            .write_u32(low_offset, lo)
            .map_err(|_| VirtioError::DeviceFault)?;
        self.window
            .write_u32(low_offset + 4, hi)
            .map_err(|_| VirtioError::DeviceFault)
    }
}

impl Transport for MmioTransport {
    fn reset(&mut self) {
        // Writing 0 to `Status` resets the device (virtio 1.1 §4.2.3.1).
        // A bounded re-read keeps the wait finite so a wedged device
        // cannot hang the boot path.
        let _ = self.window.write_u32(regs::STATUS, 0);
        for _ in 0..1_000_000 {
            match self.window.read_u32(regs::STATUS) {
                Ok(0) => break,
                _ => core::hint::spin_loop(),
            }
        }
        self.selected_queue = 0;
    }

    fn status(&self) -> Status {
        #[allow(clippy::cast_possible_truncation)]
        let bits = self.window.read_u32(regs::STATUS).unwrap_or(0) as u8;
        Status::from_bits(bits)
    }

    fn set_status(&mut self, status: Status) {
        let _ = self
            .window
            .write_u32(regs::STATUS, u32::from(status.bits()));
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
        // virtio-MMIO has no "number of queues" register; the device
        // advertises a queue's existence through a non-zero
        // `QueueNumMax` for that index. The driver discovers the count
        // by probing `queue_select` + `queue_max_size`, so the upper
        // bound is the architectural maximum a 16-bit `QueueSel` can
        // address.
        u16::MAX
    }

    fn queue_select(&mut self, queue: u16) -> Result<(), VirtioError> {
        self.window
            .write_u32(regs::QUEUE_SEL, u32::from(queue))
            .map_err(|_| VirtioError::DeviceFault)?;
        self.selected_queue = queue;
        Ok(())
    }

    fn queue_max_size(&self) -> u16 {
        #[allow(clippy::cast_possible_truncation)]
        let max = self.window.read_u32(regs::QUEUE_NUM_MAX).unwrap_or(0);
        u16::try_from(max).unwrap_or(u16::MAX)
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
        self.window
            .write_u32(regs::QUEUE_NUM, u32::from(size))
            .map_err(|_| VirtioError::DeviceFault)?;
        self.write_u64_pair(regs::QUEUE_DESC_LOW, desc)?;
        self.write_u64_pair(regs::QUEUE_DRIVER_LOW, avail)?;
        self.write_u64_pair(regs::QUEUE_DEVICE_LOW, used)?;
        self.window
            .write_u32(regs::QUEUE_READY, 1)
            .map_err(|_| VirtioError::DeviceFault)
    }

    fn notify(&mut self, queue: u16) {
        // virtio-MMIO notification is a single register write of the
        // queue index; there is no per-queue notify offset. The offset
        // is a constant below `WINDOW_MIN_LEN`, so this never faults on
        // a validly-constructed transport.
        let _ = self.window.write_u32(regs::QUEUE_NOTIFY, u32::from(queue));
    }

    fn read_config(&self, offset: usize, buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = self.window.read_u8(regs::CONFIG + offset + i).unwrap_or(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::MockHost;
    use crate::queue::SplitQueue;
    use alloc::boxed::Box;
    use core::ptr::NonNull;

    /// A buffer-backed stand-in for a modern virtio-MMIO device. The
    /// leaked, 8-byte-aligned backing storage outlives every window
    /// built over it (the test process never frees it), and the region
    /// is accessed only through the volatile [`RegisterWindow`]
    /// accessors, so the driver-side window held by [`MmioTransport`]
    /// and the test's `dev` window alias the same bytes exactly as real
    /// hardware does.
    struct FakeMmioDevice {
        base: NonNull<u8>,
        len: usize,
    }

    impl FakeMmioDevice {
        /// Build a fake whose register block spans `len` bytes (must be
        /// at least [`regs::WINDOW_MIN_LEN`]) and pre-load the identity
        /// registers a valid modern virtio-MMIO device exposes.
        fn new(len: usize) -> Self {
            let words = len.div_ceil(8);
            let boxed = alloc::vec![0u64; words.max(1)].into_boxed_slice();
            let raw = Box::leak(boxed);
            let base = NonNull::new(raw.as_mut_ptr().cast::<u8>()).expect("non-null");
            let dev = Self { base, len };
            let w = dev.window(0xD000_0000);
            w.write_u32(regs::MAGIC, regs::MAGIC_VALUE).unwrap();
            w.write_u32(regs::VERSION, regs::VERSION_MODERN).unwrap();
            w.write_u32(regs::DEVICE_ID, 2).unwrap(); // virtio-blk.
            dev
        }

        /// Build a fresh window over the region. `phys` is synthetic.
        fn window(&self, phys: u64) -> RegisterWindow {
            // SAFETY: `base` covers `len` bytes of leaked storage that
            // lives for the rest of the process; the window only ever
            // performs volatile accesses, so aliasing windows are sound
            // for the single-threaded test.
            unsafe { RegisterWindow::from_mapping(phys, self.base, self.len) }
        }

        fn dev(&self) -> RegisterWindow {
            self.window(0xD000_0000)
        }

        fn transport(&self) -> MmioTransport {
            MmioTransport::new(self.window(0xD000_0000)).expect("valid window")
        }
    }

    #[test]
    fn new_rejects_short_window() {
        let dev = FakeMmioDevice::new(regs::WINDOW_MIN_LEN);
        // A window one register short of the full register block must
        // be rejected so every infallible access stays in bounds.
        // SAFETY: the same leaked, process-lifetime backing store as
        // every other window in this test; volatile access only.
        let too_short = unsafe {
            RegisterWindow::from_mapping(0xD000_0000, dev.base, regs::WINDOW_MIN_LEN - 4)
        };
        assert!(matches!(
            MmioTransport::new(too_short),
            Err(VirtioError::DeviceFault)
        ));
    }

    #[test]
    fn new_rejects_bad_magic() {
        let dev = FakeMmioDevice::new(regs::WINDOW_MIN_LEN);
        dev.dev().write_u32(regs::MAGIC, 0xDEAD_BEEF).unwrap();
        assert!(matches!(
            MmioTransport::new(dev.window(0xD000_0000)),
            Err(VirtioError::DeviceFault)
        ));
    }

    #[test]
    fn new_rejects_legacy_version() {
        let dev = FakeMmioDevice::new(regs::WINDOW_MIN_LEN);
        dev.dev().write_u32(regs::VERSION, 1).unwrap();
        assert!(matches!(
            MmioTransport::new(dev.window(0xD000_0000)),
            Err(VirtioError::DeviceFault)
        ));
    }

    #[test]
    fn new_rejects_empty_slot() {
        let dev = FakeMmioDevice::new(regs::WINDOW_MIN_LEN);
        dev.dev().write_u32(regs::DEVICE_ID, 0).unwrap();
        assert!(matches!(
            MmioTransport::new(dev.window(0xD000_0000)),
            Err(VirtioError::DeviceFault)
        ));
    }

    #[test]
    fn status_writes_reads_and_reset_clears() {
        let dev = FakeMmioDevice::new(regs::WINDOW_MIN_LEN);
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
    fn device_and_driver_features_round_trip_both_halves() {
        let dev = FakeMmioDevice::new(regs::WINDOW_MIN_LEN);
        let c = dev.dev();
        // Plant the low half; the fake does not window DeviceFeatures,
        // so device_features() observes the last-written value for both
        // selects. Assert the select wiring instead via driver writes.
        c.write_u32(regs::DEVICE_FEATURES, 0x0000_00FF).unwrap();
        let mut t = dev.transport();
        assert_eq!(t.device_features(), 0x0000_00FF_0000_00FF);
        t.set_driver_features(0x0000_0001_0000_0002);
        // After the two-step write the select register holds the high
        // index and the feature register holds the high half.
        assert_eq!(c.read_u32(regs::DRIVER_FEATURES_SEL).unwrap(), 1);
        assert_eq!(c.read_u32(regs::DRIVER_FEATURES).unwrap(), 1);
    }

    #[test]
    fn queue_select_writes_register() {
        let dev = FakeMmioDevice::new(regs::WINDOW_MIN_LEN);
        let mut t = dev.transport();
        t.queue_select(3).unwrap();
        assert_eq!(dev.dev().read_u32(regs::QUEUE_SEL).unwrap(), 3);
    }

    #[test]
    fn queue_set_programs_registers_and_marks_ready() {
        let dev = FakeMmioDevice::new(regs::WINDOW_MIN_LEN);
        let c = dev.dev();
        c.write_u32(regs::QUEUE_NUM_MAX, 8).unwrap();
        let mut t = dev.transport();
        t.queue_select(0).unwrap();
        assert_eq!(t.queue_max_size(), 8);
        t.queue_set(8, 0x1234_5678_9ABC_DEF0, 0x0011_2233, 0x4455_6677)
            .unwrap();
        assert_eq!(c.read_u32(regs::QUEUE_NUM).unwrap(), 8);
        assert_eq!(c.read_u32(regs::QUEUE_DESC_LOW).unwrap(), 0x9ABC_DEF0);
        assert_eq!(c.read_u32(regs::QUEUE_DESC_HIGH).unwrap(), 0x1234_5678);
        assert_eq!(c.read_u32(regs::QUEUE_DRIVER_LOW).unwrap(), 0x0011_2233);
        assert_eq!(c.read_u32(regs::QUEUE_DEVICE_LOW).unwrap(), 0x4455_6677);
        assert_eq!(c.read_u32(regs::QUEUE_READY).unwrap(), 1);
    }

    #[test]
    fn queue_set_rejects_oversize() {
        let dev = FakeMmioDevice::new(regs::WINDOW_MIN_LEN);
        dev.dev().write_u32(regs::QUEUE_NUM_MAX, 8).unwrap();
        let mut t = dev.transport();
        t.queue_select(0).unwrap();
        assert_eq!(
            t.queue_set(16, 1, 2, 3),
            Err(VirtioError::QueueSizeTooLarge)
        );
        assert_eq!(t.queue_set(0, 1, 2, 3), Err(VirtioError::QueueSizeTooLarge));
    }

    #[test]
    fn notify_writes_queue_index() {
        let dev = FakeMmioDevice::new(regs::WINDOW_MIN_LEN);
        let mut t = dev.transport();
        t.notify(2);
        assert_eq!(dev.dev().read_u32(regs::QUEUE_NOTIFY).unwrap(), 2);
    }

    #[test]
    fn read_config_reads_config_window_and_zero_fills_overflow() {
        let dev = FakeMmioDevice::new(regs::CONFIG + 8);
        let d = dev.dev();
        d.write_u32(regs::CONFIG, 0x0403_0201).unwrap();
        d.write_u32(regs::CONFIG + 4, 0x0807_0605).unwrap();
        let t = dev.transport();
        let mut buf = [0u8; 4];
        t.read_config(2, &mut buf);
        assert_eq!(buf, [3, 4, 5, 6]);
        // Reading past the window zero-fills.
        let mut over = [0xCDu8; 4];
        t.read_config(6, &mut over);
        assert_eq!(over, [7, 8, 0, 0]);
    }

    #[test]
    fn split_queue_drives_mmio_transport() {
        // Prove the transport integrates with the generic split-queue
        // setup path: SplitQueue::new selects, sizes, and programs the
        // queue through the MmioTransport.
        let dev = FakeMmioDevice::new(regs::WINDOW_MIN_LEN);
        dev.dev().write_u32(regs::QUEUE_NUM_MAX, 8).unwrap();
        let mut t = dev.transport();
        let host: &'static MockHost = Box::leak(Box::new(MockHost::new()));
        let q = SplitQueue::new(&mut t, host, 0, 8).expect("queue setup");
        assert_eq!(q.size(), 8);
        let c = dev.dev();
        let lo = c.read_u32(regs::QUEUE_DESC_LOW).unwrap();
        let hi = c.read_u32(regs::QUEUE_DESC_HIGH).unwrap();
        assert_ne!((u64::from(hi) << 32) | u64::from(lo), 0);
        assert_eq!(c.read_u32(regs::QUEUE_READY).unwrap(), 1);
    }
}
