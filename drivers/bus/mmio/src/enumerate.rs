//! DTB-driven enumeration of virtio-MMIO transport slots.
//!
//! The walker visits every node whose `compatible` property contains
//! the string `"virtio,mmio"`, reads the slot's `reg` property to
//! obtain the physical base address (and window length), then probes
//! the four-register identifier window through [`MmioRead`]:
//!
//! ```text
//!  offset 0x000 : MagicValue   (must equal `"virt"` LE = 0x74726976)
//!  offset 0x004 : Version      (must equal 1 or 2)
//!  offset 0x008 : DeviceID     (0 means "slot empty")
//!  offset 0x00C : VendorID
//! ```
//!
//! Slots whose `MagicValue` mismatches or whose `DeviceID` is 0 are
//! skipped silently — this is exactly how QEMU's `virt` machine
//! presents unattached transports (`hw/virtio/virtio-mmio.c`).

// Same `dead_code` rationale as the PCI driver crate.
#![allow(dead_code)]

use tairix_abi::driver::bus::BusDevice;
use tairix_abi::{DriverError, MmioMapError, MmioMapper, RegisterWindow};
use tairix_fdt::Fdt;

use crate::transport::MmioRead;

/// The string the walker matches against `compatible`.
pub const VIRTIO_MMIO_COMPATIBLE: &str = "virtio,mmio";

/// `MagicValue` byte sequence — `"virt"` as a little-endian word.
pub const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;

/// Vendor field used in [`BusDevice::vendor`] for virtio-MMIO
/// transports. Virtio over MMIO does not carry a PCI-style vendor
/// ID, so the driver reports the canonical Red Hat / virtio vendor
/// ID (`0x1AF4`) read from the `VendorID` register; if the device
/// fails to populate that register the walker substitutes the
/// well-known fallback below — matching what `virtio-mmio.c` in
/// QEMU writes when no upper driver has attached.
pub const VIRTIO_MMIO_DEFAULT_VENDOR: u32 = 0x554D_4551; // "QEMU"

const REG_MAGIC: u64 = 0x000;
const REG_VERSION: u64 = 0x004;
const REG_DEVICE_ID: u64 = 0x008;
const REG_VENDOR_ID: u64 = 0x00C;

/// The MMIO bus driver instance.
///
/// Bound to a parsed [`Fdt`] (`'dtb`) and a [`MmioRead`] reader; the
/// type is `pub(crate)` and reached from outside
/// only via `dyn Bus`.
pub struct Mmio<'dtb, T: MmioRead> {
    dtb: Fdt<'dtb>,
    reader: T,
}

impl<'dtb, T: MmioRead> Mmio<'dtb, T> {
    /// Construct an [`Mmio`] over a pre-parsed device-tree blob and
    /// volatile reader.
    pub const fn new(dtb: Fdt<'dtb>, reader: T) -> Self {
        Self { dtb, reader }
    }

    /// Enumerate every populated virtio-MMIO slot into `out`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `out` cannot hold every
    ///   discovered slot.
    /// * [`DriverError::DeviceFault`] if the DTB walk encounters a
    ///   malformed `compatible` or `reg` property; the walker fails
    ///   closed so a hostile blob cannot cause silent
    ///   under-enumeration.
    pub fn enumerate_into(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
        let mut count = 0usize;
        let mut overflow = false;
        for node in self.dtb.nodes() {
            let node = node.map_err(|_| DriverError::DeviceFault)?;
            if !node.is_compatible(VIRTIO_MMIO_COMPATIBLE) {
                continue;
            }
            // `reg` carries one `<base, length>` pair for `virt`-style
            // platforms (#address-cells = 2, #size-cells = 2).
            let reg = node.property("reg").ok_or(DriverError::DeviceFault)?;
            let base = reg.read_be_u64(0).map_err(|_| DriverError::DeviceFault)?;
            // length is read but not currently propagated; the size
            // field on `BusDevice` is the bus-defined `class` slot.
            let _length = reg.read_be_u64(8).map_err(|_| DriverError::DeviceFault)?;

            let magic = self.reader.read32(base + REG_MAGIC);
            if magic != VIRTIO_MMIO_MAGIC {
                continue;
            }
            let device_id = self.reader.read32(base + REG_DEVICE_ID);
            if device_id == 0 {
                continue;
            }
            let version = self.reader.read32(base + REG_VERSION);
            let vendor_raw = self.reader.read32(base + REG_VENDOR_ID);
            let vendor = if vendor_raw == 0 {
                VIRTIO_MMIO_DEFAULT_VENDOR
            } else {
                vendor_raw
            };

            // The `class` slot carries the virtio transport version
            // (1 or 2); that information is needed by virtio-blk /
            // virtio-net to choose the legacy vs. modern protocol.
            // Truncating to 16 bits is lossless — version is 1 or 2.
            let class = (version & 0xFFFF) as u16;
            let entry = BusDevice {
                vendor,
                device: device_id,
                class,
                reserved0: 0,
                address: base,
            };
            if count < out.len() {
                out[count] = entry;
            } else {
                overflow = true;
            }
            count += 1;
        }
        if overflow {
            Err(DriverError::BufferTooSmall)
        } else {
            Ok(count)
        }
    }

    /// Resolve the virtio-MMIO transport slot at physical `base` and
    /// ask the kernel `mapper` to map its register window, returning
    /// the resulting [`RegisterWindow`].
    ///
    /// This is the Stage 4.D Item 3 hand-off for the MMIO bus: the
    /// driver reads the slot's `<base, length>` pair from the device
    /// tree and asks the kernel's MMIO-map facility for a window over
    /// it. The driver never synthesises a pointer — the kernel
    /// allocates and validates the mapping. The
    /// returned window is what the bus driver hands to the virtio
    /// transport's `MmioBackend`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — no `virtio,mmio` slot whose `reg`
    ///   base equals `base` exists in the device tree.
    /// * [`DriverError::DeviceFault`] — the matching node's `reg`
    ///   property is malformed (fails closed, like
    ///   [`Self::enumerate_into`]).
    /// * [`DriverError::LengthOutOfRange`] — the slot length does not
    ///   fit in `usize` on this target.
    /// * [`DriverError::PermissionDenied`] — the caller does not hold
    ///   [`CapabilityId::MMIO_MAP`](tairix_abi::CapabilityId::MMIO_MAP)
    ///   (propagated from the mapper).
    ///
    /// # Capabilities
    ///
    /// The `mapper` enforces
    /// [`CapabilityId::MMIO_MAP`](tairix_abi::CapabilityId::MMIO_MAP).
    pub fn map_slot_window(
        &self,
        base: u64,
        mapper: &dyn MmioMapper,
    ) -> Result<RegisterWindow, DriverError> {
        let length = self.slot_window_len(base)?;
        let len = usize::try_from(length).map_err(|_| DriverError::LengthOutOfRange)?;
        mapper
            .map_window(base, len)
            .map_err(MmioMapError::as_driver_error)
    }

    /// Return the length, in bytes, of the register window of the
    /// `virtio,mmio` transport slot whose `reg` base equals `base`.
    ///
    /// This is the discovered slot extent the device tree `reg`
    /// `<base, length>` pair declares (a discovered
    /// value, never a literal). It is the unmapped half of
    /// [`Self::map_slot_window`]: the bootstrap-floor discovery walk
    /// records it as a discovered virtio device node's MMIO resource so a
    /// user-space driver autoloaded against that node is granted a window
    /// of exactly the slot's size. It reads no device
    /// state and maps nothing, so it needs no [`MmioMapper`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — no `virtio,mmio` slot whose `reg`
    ///   base equals `base` exists in the device tree.
    /// * [`DriverError::DeviceFault`] — the matching node's `reg`
    ///   property is malformed (fails closed, like
    ///   [`Self::enumerate_into`]).
    pub fn slot_window_len(&self, base: u64) -> Result<u64, DriverError> {
        for node in self.dtb.nodes() {
            let node = node.map_err(|_| DriverError::DeviceFault)?;
            if !node.is_compatible(VIRTIO_MMIO_COMPATIBLE) {
                continue;
            }
            let reg = node.property("reg").ok_or(DriverError::DeviceFault)?;
            let slot_base = reg.read_be_u64(0).map_err(|_| DriverError::DeviceFault)?;
            if slot_base != base {
                continue;
            }
            return reg.read_be_u64(8).map_err(|_| DriverError::DeviceFault);
        }
        Err(DriverError::NotFound)
    }
}
