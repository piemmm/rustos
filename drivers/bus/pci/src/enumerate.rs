//! Enumeration core: bus walk, capability-list walk, BAR sizing.
//!
//! All logic is parameterised over [`ConfigSpace`] so the host-side
//! tests can substitute a table-driven mock that reproduces QEMU's
//! `q35` PCI tree byte for byte. The walk is bounded by hardware
//! limits (256 buses × 32 devices × 8 functions, 6 BARs per type-0
//! function, 48 capability entries — the legacy 256-byte
//! configuration space cannot fit more) so it terminates without
//! external timeouts.
//
// Same `dead_code` rationale as `config.rs` / `mech_one.rs`.
#![allow(dead_code)]

use rustos_abi::driver::bus::BusDevice;
use rustos_abi::{DriverError, MmioMapError, MmioMapper, MsiMessage, RegisterWindow, WindowError};

use crate::config::{
    BarDescriptor, BarKind, Capability, ConfigAddress, ConfigSpace, CAP_ID_VENDOR,
    VIRTIO_CFG_NOTIFY,
};

/// Maximum number of BAR slots a type-0 PCI function exposes
/// (PCI Local Bus 3.0 §6.1).
const MAX_BARS: usize = 6;

/// Vendor-ID sentinel returned by the host bridge when no function
/// is present at a given `(bus, device, function)`.
const VENDOR_INVALID: u16 = 0xFFFF;

/// Status-register bit 4 — "Capabilities List".
const STATUS_CAP_LIST: u16 = 1 << 4;

/// Maximum number of capability-list entries the walker will follow.
///
/// The 256-byte legacy configuration space has at most ~48 dword
/// slots available for capabilities; the bound is set above that to
/// catch any walker bug (a circular `next` pointer) without spinning
/// forever.
const CAP_LIST_HARD_LIMIT: usize = 64;

/// MSI-X table entry size in bytes (PCI Local Bus 3.0 §6.8.2.9):
/// message address (8) + message data (4) + vector control (4).
const MSIX_ENTRY_LEN: usize = 16;

/// MSI-X Message Control "MSI-X Enable" bit. The Message Control
/// register occupies the high 16 bits of the capability header dword,
/// so its bit 15 lands at bit 31 of the dword.
const MSIX_CTRL_ENABLE: u32 = 1 << 31;

/// MSI-X Message Control "Function Mask" bit (bit 14 of Message
/// Control → bit 30 of the header dword); cleared so unmasked table
/// entries deliver.
const MSIX_CTRL_FUNCTION_MASK: u32 = 1 << 30;

/// PCI Command register "Memory Space Enable" bit (PCI Local Bus 3.0
/// §6.2.2). Set so the function decodes accesses to its memory BARs —
/// required to reach the virtio register windows and the MSI-X table.
const CMD_MEMORY_SPACE: u32 = 1 << 1;

/// PCI Command register "Bus Master Enable" bit (PCI Local Bus 3.0
/// §6.2.2). Set so the function may issue upstream memory transactions
/// — required both for virtqueue DMA and for MSI-X message delivery
/// (an MSI-X interrupt is itself an upstream memory write).
const CMD_BUS_MASTER: u32 = 1 << 2;

/// The PCI bus driver instance.
///
/// Holds the [`ConfigSpace`] backend; everything else is
/// constructor-injected. The type is `pub(crate)` per `AGENTS.md` §8
/// — outside callers reach the enumeration through `dyn Bus`.
pub struct Pci<C: ConfigSpace> {
    config: C,
}

impl<C: ConfigSpace> Pci<C> {
    /// Construct a new [`Pci`] wired to `config`.
    pub const fn new(config: C) -> Self {
        Self { config }
    }

    /// Enumerate every responding function on every bus into `out`.
    ///
    /// Returns the number of entries written. If `out.len()` is
    /// smaller than the number of devices discovered, the method
    /// fills `out` and returns [`DriverError::BufferTooSmall`] —
    /// matching the [`Bus::enumerate`](rustos_abi::driver::bus::Bus)
    /// contract exactly.
    pub fn enumerate_into(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
        let mut count = 0usize;
        let mut overflow = false;
        for bus_u8 in 0u8..=255 {
            for device in 0u8..32 {
                let multi = self.is_multifunction(bus_u8, device);
                let max_fn = if multi { 8 } else { 1 };
                for function in 0..max_fn {
                    let addr = ConfigAddress {
                        bus: bus_u8,
                        device,
                        function,
                        register: 0,
                    };
                    let id = self.config.read32(addr);
                    // Truncating the low 16 bits of a configuration
                    // dword is lossless by definition (vendor ID is
                    // 16 bits wide per PCI Local Bus 3.0 §6.2.1).
                    let vendor = low_u16(id);
                    if vendor == VENDOR_INVALID {
                        continue;
                    }
                    let device_id = low_u16(id >> 16);
                    let class = self.read_class(addr);
                    let entry = BusDevice {
                        vendor: u32::from(vendor),
                        device: u32::from(device_id),
                        class,
                        reserved0: 0,
                        address: addr.pack_bdf(),
                    };
                    if count < out.len() {
                        out[count] = entry;
                    } else {
                        overflow = true;
                    }
                    count += 1;
                }
            }
        }
        if overflow {
            Err(DriverError::BufferTooSmall)
        } else {
            Ok(count)
        }
    }

    /// Walk the function's capability list into `out`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if the function does not advertise
    ///   any capability list (status bit 4 clear).
    /// * [`DriverError::BufferTooSmall`] if `out` cannot hold every
    ///   discovered capability.
    /// * [`DriverError::DeviceFault`] if the cap-list walker exceeds
    ///   [`CAP_LIST_HARD_LIMIT`] — almost certainly a circular
    ///   `next` pointer planted by a malfunctioning device.
    pub fn capabilities(&self, bdf: u64, out: &mut [Capability]) -> Result<usize, DriverError> {
        let addr = unpack_bdf(bdf, 0);
        let status_cmd = self.config.read32(addr_with_reg(addr, 1));
        let status = low_u16(status_cmd >> 16);
        if status & STATUS_CAP_LIST == 0 {
            return Err(DriverError::NotFound);
        }
        // Cap pointer at config-space offset 0x34 (register dword 13).
        let cap_ptr_dword = self.config.read32(addr_with_reg(addr, 13));
        let mut cap_offset = low_u8(cap_ptr_dword & 0xFC);
        let mut count = 0usize;
        let mut overflow = false;
        let mut steps = 0usize;
        while steps < CAP_LIST_HARD_LIMIT {
            steps += 1;
            if cap_offset == 0 {
                return if overflow {
                    Err(DriverError::BufferTooSmall)
                } else {
                    Ok(count)
                };
            }
            let header_addr = addr_with_byte_offset(addr, cap_offset);
            let header = self.config.read32(header_addr);
            let cap_id = low_u8(header);
            let next = low_u8((header >> 8) & 0xFC);
            let msg_ctrl = low_u16(header >> 16);
            let entry = match cap_id {
                0x05 => decode_msi(self, addr, cap_offset, msg_ctrl),
                0x11 => decode_msix(self, addr, cap_offset, msg_ctrl),
                CAP_ID_VENDOR => decode_virtio(self, addr, cap_offset, msg_ctrl),
                id => Capability::Other {
                    offset: cap_offset,
                    id,
                },
            };
            if count < out.len() {
                out[count] = entry;
            } else {
                overflow = true;
            }
            count += 1;
            cap_offset = next;
        }
        // Loop budget exhausted without hitting a `next == 0`
        // terminator — assume a malfunctioning device.
        Err(DriverError::DeviceFault)
    }

    /// Decode every BAR slot of a *type-0* function into `out`.
    ///
    /// Type-1 (PCI-to-PCI bridge) and type-2 (`CardBus`) headers are
    /// recognised but produce no BAR records — they are out of scope
    /// for Stage 4 (`AGENTS.md` §8: only the surface the first
    /// drivers need).
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `out` cannot hold every
    ///   used BAR slot.
    /// * [`DriverError::Unsupported`] if the function has a non-type-0
    ///   header.
    pub fn bars(&self, bdf: u64, out: &mut [BarDescriptor]) -> Result<usize, DriverError> {
        let addr = unpack_bdf(bdf, 0);
        let header_type_byte = low_u8(self.config.read32(addr_with_reg(addr, 3)) >> 16);
        if header_type_byte & 0x7F != 0 {
            return Err(DriverError::Unsupported);
        }
        let mut count = 0usize;
        let mut overflow = false;
        let mut index: u8 = 0;
        while index < 6 {
            let bar_reg = 4 + index; // BAR0 lives at dword 4.
            let lo = self.config.read32(addr_with_reg(addr, bar_reg));
            if lo == 0 {
                index += 1;
                continue;
            }
            let is_io = lo & 0x1 != 0;
            let (kind, base, slot_advance, prefetchable) = if is_io {
                let base = u64::from(lo & 0xFFFF_FFFC);
                (BarKind::Io, base, 1u8, false)
            } else {
                let bits_21 = (lo >> 1) & 0x3;
                let pref = (lo >> 3) & 0x1 != 0;
                if bits_21 == 0x2 {
                    // 64-bit BAR — pair with the next slot.
                    let high = self.config.read32(addr_with_reg(addr, bar_reg + 1));
                    let base = (u64::from(high) << 32) | u64::from(lo & 0xFFFF_FFF0);
                    (BarKind::Memory64, base, 2u8, pref)
                } else {
                    let base = u64::from(lo & 0xFFFF_FFF0);
                    (BarKind::Memory32, base, 1u8, pref)
                }
            };
            // Size probe: write FFFFFFFF, read back, restore.
            self.config
                .write32(addr_with_reg(addr, bar_reg), 0xFFFF_FFFF);
            let probe = self.config.read32(addr_with_reg(addr, bar_reg));
            self.config.write32(addr_with_reg(addr, bar_reg), lo);
            let mask = if is_io {
                probe & 0xFFFF_FFFC
            } else {
                probe & 0xFFFF_FFF0
            };
            let size = if mask == 0 {
                0
            } else {
                (!u64::from(mask) + 1) & 0xFFFF_FFFF
            };
            let descriptor = BarDescriptor {
                index,
                kind,
                base,
                size,
                prefetchable,
            };
            if count < out.len() {
                out[count] = descriptor;
            } else {
                overflow = true;
            }
            count += 1;
            index += slot_advance;
        }
        if overflow {
            Err(DriverError::BufferTooSmall)
        } else {
            Ok(count)
        }
    }

    /// Resolve the memory BAR at `bar_index` on function `bdf` and ask
    /// the kernel `mapper` to map it, returning the resulting
    /// [`RegisterWindow`].
    ///
    /// This is the Stage 4.D Item 3 hand-off: the PCI bus driver
    /// resolves the device's register-block physical base and length
    /// from configuration space and asks the kernel's MMIO-map
    /// facility for a window over it. The driver never synthesises a
    /// pointer — the kernel allocates and validates the mapping
    /// (`AGENTS.md` §4). The returned window is what the bus driver
    /// hands to the virtio transport's `PciBackend`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — no BAR with `bar_index` exists,
    ///   or the BAR is unused (`size == 0`).
    /// * [`DriverError::Unsupported`] — the BAR is an I/O-port BAR,
    ///   which is reached through port I/O rather than a mapped
    ///   register window, or the function is not a type-0 header.
    /// * [`DriverError::LengthOutOfRange`] — the BAR size does not fit
    ///   in `usize` on this target.
    /// * [`DriverError::PermissionDenied`] — the caller does not hold
    ///   [`CapabilityId::MMIO_MAP`](rustos_abi::CapabilityId::MMIO_MAP)
    ///   (propagated from the mapper).
    ///
    /// # Capabilities
    ///
    /// The `mapper` enforces
    /// [`CapabilityId::MMIO_MAP`](rustos_abi::CapabilityId::MMIO_MAP).
    pub fn map_bar_window(
        &self,
        bdf: u64,
        bar_index: u8,
        mapper: &dyn MmioMapper,
    ) -> Result<RegisterWindow, DriverError> {
        let bar = self.resolve_bar(bdf, bar_index)?;
        // An I/O-port BAR is reached through port I/O, not a mapped
        // register window; refuse to pretend otherwise.
        if matches!(bar.kind, BarKind::Io) {
            return Err(DriverError::Unsupported);
        }
        if bar.size == 0 {
            return Err(DriverError::NotFound);
        }
        let len = usize::try_from(bar.size).map_err(|_| DriverError::LengthOutOfRange)?;
        mapper
            .map_window(bar.base, len)
            .map_err(MmioMapError::as_driver_error)
    }

    /// Enable memory-space decoding and bus-mastering on function
    /// `bdf` (PCI Local Bus 3.0 §6.2.2).
    ///
    /// Firmware leaves the Bus Master Enable bit clear, so a function
    /// whose register block is mapped but whose bus-master bit is clear
    /// can never issue the upstream memory transactions its DMA rings
    /// depend on. A DMA-driving driver (virtio, xHCI) calls this once
    /// before programming the device. [`route_msix`](Self::route_msix)
    /// folds the same activation into its own hand-off, so the two
    /// share one definition (`AGENTS.md` §2.2).
    ///
    /// The in-tree [`ConfigSpace`] backends' accesses are infallible,
    /// so this cannot fail; the [`PciBus`](rustos_abi::driver::pci::PciBus)
    /// trait method wraps the result in `Ok` and reserves the error
    /// arm for a future fallible transport.
    pub fn enable_bus_master(&self, bdf: u64) {
        let cmd_addr = addr_with_reg(unpack_bdf(bdf, 0), 1);
        let command = self.config.read32(cmd_addr);
        // Preserve the low-16 command bits, drop the high-16 status
        // bits to 0 (RW1C: a 0 write never clears a status bit), then
        // OR in memory-space + bus-master enable.
        let command = (command & 0xFFFF) | CMD_MEMORY_SPACE | CMD_BUS_MASTER;
        self.config.write32(cmd_addr, command);
    }

    /// Resolve the virtio-1.x configuration structure of kind
    /// `cfg_type` on function `bdf` and ask the kernel `mapper` to map
    /// it, returning the resulting [`RegisterWindow`].
    ///
    /// This is the boot-time hand-off the virtio PCI transport needs:
    /// the bus driver walks the function's capability list, locates
    /// the vendor-specific virtio capability of the requested
    /// `cfg_type` (one of [`VIRTIO_CFG_COMMON`](crate::config::VIRTIO_CFG_COMMON),
    /// [`VIRTIO_CFG_NOTIFY`],
    /// [`VIRTIO_CFG_ISR`](crate::config::VIRTIO_CFG_ISR), or
    /// [`VIRTIO_CFG_DEVICE`](crate::config::VIRTIO_CFG_DEVICE)),
    /// resolves the `(bar, bar_offset, length)` triple to a physical
    /// base, and asks the kernel's MMIO-map facility for a window over
    /// exactly that region. The driver never synthesises a pointer —
    /// the kernel allocates and validates the mapping (`AGENTS.md` §4).
    /// The four windows so produced are what
    /// `PciTransport::new` consumes.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — the function advertises no virtio
    ///   capability of `cfg_type`, or the underlying BAR is unused.
    /// * [`DriverError::Unsupported`] — the structure lives in an
    ///   I/O-port BAR, which is reached through port I/O rather than a
    ///   mapped register window, or the function is not a type-0 header.
    /// * [`DriverError::OutOfRange`] — the structure's
    ///   `bar_offset + length` exceeds the resolved BAR size.
    /// * [`DriverError::LengthOutOfRange`] — the region length does not
    ///   fit in `usize` on this target.
    /// * [`DriverError::PermissionDenied`] — the caller does not hold
    ///   [`CapabilityId::MMIO_MAP`](rustos_abi::CapabilityId::MMIO_MAP)
    ///   (propagated from the mapper).
    ///
    /// # Capabilities
    ///
    /// The `mapper` enforces
    /// [`CapabilityId::MMIO_MAP`](rustos_abi::CapabilityId::MMIO_MAP).
    pub fn map_virtio_window(
        &self,
        bdf: u64,
        cfg_type: u8,
        mapper: &dyn MmioMapper,
    ) -> Result<RegisterWindow, DriverError> {
        let (bar_index, bar_offset, length) = self.find_virtio_region(bdf, cfg_type)?;
        let bar = self.resolve_bar(bdf, bar_index)?;
        if matches!(bar.kind, BarKind::Io) {
            return Err(DriverError::Unsupported);
        }
        let end = u64::from(bar_offset)
            .checked_add(u64::from(length))
            .ok_or(DriverError::OutOfRange)?;
        if length == 0 || end > bar.size {
            return Err(DriverError::OutOfRange);
        }
        // `bar.base + bar_offset` stays within the BAR's reserved span
        // (checked above), so the addition cannot overflow the address.
        let phys_base = bar
            .base
            .checked_add(u64::from(bar_offset))
            .ok_or(DriverError::OutOfRange)?;
        let len = usize::try_from(length).map_err(|_| DriverError::LengthOutOfRange)?;
        mapper
            .map_window(phys_base, len)
            .map_err(MmioMapError::as_driver_error)
    }

    /// Read the `notify_off_multiplier` from the function's virtio
    /// notification capability.
    ///
    /// Returned alongside the four windows from [`map_virtio_window`]
    /// to populate `PciTransport`'s notification scale (virtio 1.x
    /// §4.1.4.4).
    ///
    /// [`map_virtio_window`]: Self::map_virtio_window
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — the function advertises no virtio
    ///   notification capability, or no capability list at all.
    /// * [`DriverError::BufferTooSmall`] / [`DriverError::DeviceFault`]
    ///   — propagated from the capability-list walk.
    pub fn virtio_notify_off_multiplier(&self, bdf: u64) -> Result<u32, DriverError> {
        let mut caps = [Capability::Other { offset: 0, id: 0 }; CAP_LIST_HARD_LIMIT];
        let n = self.capabilities(bdf, &mut caps)?;
        caps[..n]
            .iter()
            .find_map(|c| match *c {
                Capability::VirtioNotify {
                    notify_off_multiplier,
                    ..
                } => Some(notify_off_multiplier),
                _ => None,
            })
            .ok_or(DriverError::NotFound)
    }

    /// Program MSI-X table `entry` of function `bdf` with `message`,
    /// unmask the entry, and enable MSI-X on the function.
    ///
    /// This is the interrupt-routing hand-off a virtio (or any
    /// MSI-X-capable) driver needs: the kernel's interrupt controller
    /// mints an [`MsiMessage`] for a chosen vector/destination, and the
    /// bus driver writes it into the device's table and flips the
    /// enable bit. The driver never synthesises a pointer — the table
    /// write goes through a kernel-mapped [`RegisterWindow`] obtained
    /// from `mapper` (`AGENTS.md` §4).
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — the function advertises no MSI-X
    ///   capability, or no capability list at all.
    /// * [`DriverError::OutOfRange`] — `entry` is beyond the function's
    ///   MSI-X table, or the addressed entry overruns its BAR.
    /// * [`DriverError::Unsupported`] — the table lives in an I/O-port
    ///   BAR, which is not memory-mappable, or the function is not a
    ///   type-0 header.
    /// * [`DriverError::LengthOutOfRange`] — the region length does not
    ///   fit in `usize` on this target (propagated from the mapper).
    /// * [`DriverError::PermissionDenied`] — the caller does not hold
    ///   [`CapabilityId::MMIO_MAP`](rustos_abi::CapabilityId::MMIO_MAP)
    ///   (propagated from the mapper).
    /// * [`DriverError::BufferTooSmall`] / [`DriverError::DeviceFault`]
    ///   — propagated from the capability-list walk.
    ///
    /// # Capabilities
    ///
    /// The `mapper` enforces
    /// [`CapabilityId::MMIO_MAP`](rustos_abi::CapabilityId::MMIO_MAP).
    pub fn route_msix(
        &self,
        bdf: u64,
        entry: u16,
        message: MsiMessage,
        mapper: &dyn MmioMapper,
    ) -> Result<(), DriverError> {
        // Activate the function before touching its MSI-X table: enable
        // memory-space decoding (so the table BAR responds) and bus
        // mastering (so both virtqueue DMA and the MSI-X message write
        // can reach the host bridge). Firmware leaves bus mastering off
        // by default, so a device whose interrupt was "routed" but whose
        // bus-master bit is clear would never deliver — fold the enable
        // into the same activation step (PCI Local Bus 3.0 §6.2.2).
        self.enable_bus_master(bdf);

        let (cap_offset, table_size, table_bar, table_offset) = self.find_msix(bdf)?;
        if entry >= table_size {
            return Err(DriverError::OutOfRange);
        }
        let bar = self.resolve_bar(bdf, table_bar)?;
        if matches!(bar.kind, BarKind::Io) {
            return Err(DriverError::Unsupported);
        }
        // Byte offset of this entry within the table BAR, bounds-checked
        // against the BAR's reserved span before any access.
        let entry_off = u64::from(table_offset)
            .checked_add(u64::from(entry).wrapping_mul(MSIX_ENTRY_LEN as u64))
            .ok_or(DriverError::OutOfRange)?;
        let end = entry_off
            .checked_add(MSIX_ENTRY_LEN as u64)
            .ok_or(DriverError::OutOfRange)?;
        if end > bar.size {
            return Err(DriverError::OutOfRange);
        }
        let phys = bar
            .base
            .checked_add(entry_off)
            .ok_or(DriverError::OutOfRange)?;
        let window = mapper
            .map_window(phys, MSIX_ENTRY_LEN)
            .map_err(MmioMapError::as_driver_error)?;
        // MSI-X table entry layout (PCI Local Bus 3.0 §6.8.2.9):
        // message address low / high, message data, vector control.
        // Program address + data first, then clear the entry's mask
        // bit (vector control bit 0) by writing zero.
        let addr_lo = (message.address & 0xFFFF_FFFF) as u32;
        let addr_hi = (message.address >> 32) as u32;
        window
            .write_u32(0, addr_lo)
            .map_err(WindowError::as_driver_error)?;
        window
            .write_u32(4, addr_hi)
            .map_err(WindowError::as_driver_error)?;
        window
            .write_u32(8, message.data)
            .map_err(WindowError::as_driver_error)?;
        window
            .write_u32(12, 0)
            .map_err(WindowError::as_driver_error)?;
        // Enable MSI-X function-wide and clear the function mask so the
        // freshly-unmasked entry can deliver. The Message Control
        // register lives in the high 16 bits of the capability header
        // dword; cap_id / next-pointer in the low 16 bits are
        // read-only and ignore writes.
        let header_addr = addr_with_byte_offset(unpack_bdf(bdf, 0), cap_offset);
        let header = self.config.read32(header_addr);
        let updated = (header | MSIX_CTRL_ENABLE) & !MSIX_CTRL_FUNCTION_MASK;
        self.config.write32(header_addr, updated);
        Ok(())
    }

    /// Locate the function's MSI-X capability, returning its
    /// `(cap_offset, table_size, table_bar, table_offset)`.
    fn find_msix(&self, bdf: u64) -> Result<(u8, u16, u8, u32), DriverError> {
        let mut caps = [Capability::Other { offset: 0, id: 0 }; CAP_LIST_HARD_LIMIT];
        let n = self.capabilities(bdf, &mut caps)?;
        caps[..n]
            .iter()
            .find_map(|c| match *c {
                Capability::MsiX {
                    offset,
                    table_size,
                    table_bar,
                    table_offset,
                    ..
                } => Some((offset, table_size, table_bar, table_offset)),
                _ => None,
            })
            .ok_or(DriverError::NotFound)
    }

    /// Locate the virtio config region of `cfg_type`, returning its
    /// `(bar_index, bar_offset, length)`.
    fn find_virtio_region(&self, bdf: u64, cfg_type: u8) -> Result<(u8, u32, u32), DriverError> {
        let mut caps = [Capability::Other { offset: 0, id: 0 }; CAP_LIST_HARD_LIMIT];
        let n = self.capabilities(bdf, &mut caps)?;
        caps[..n]
            .iter()
            .find_map(|c| match *c {
                Capability::Virtio {
                    cfg_type: ct,
                    bar,
                    bar_offset,
                    length,
                    ..
                } if ct == cfg_type => Some((bar, bar_offset, length)),
                Capability::VirtioNotify {
                    bar,
                    bar_offset,
                    length,
                    ..
                } if cfg_type == VIRTIO_CFG_NOTIFY => Some((bar, bar_offset, length)),
                _ => None,
            })
            .ok_or(DriverError::NotFound)
    }

    /// Resolve a single BAR descriptor by index.
    fn resolve_bar(&self, bdf: u64, bar_index: u8) -> Result<BarDescriptor, DriverError> {
        let mut descriptors = [BarDescriptor {
            index: 0,
            kind: BarKind::Memory32,
            base: 0,
            size: 0,
            prefetchable: false,
        }; MAX_BARS];
        let n = self.bars(bdf, &mut descriptors)?;
        descriptors[..n]
            .iter()
            .copied()
            .find(|b| b.index == bar_index)
            .ok_or(DriverError::NotFound)
    }

    fn is_multifunction(&self, bus: u8, device: u8) -> bool {
        let addr = ConfigAddress {
            bus,
            device,
            function: 0,
            register: 3,
        };
        // Reading function 0 first: if vendor is invalid the slot
        // is empty and we needn't probe higher functions.
        let id_addr = ConfigAddress {
            register: 0,
            ..addr
        };
        let id = self.config.read32(id_addr);
        if low_u16(id) == VENDOR_INVALID {
            return false;
        }
        let header_type = low_u8(self.config.read32(addr) >> 16);
        header_type & 0x80 != 0
    }

    fn read_class(&self, base_addr: ConfigAddress) -> u16 {
        // Class is the upper 16 bits of dword 2: class code (high
        // byte) plus subclass code (low byte). Programming interface
        // and revision ID live in the lower 16 bits and are not part
        // of the [`BusDevice::class`] field for `abi-v1`.
        let dword = self.config.read32(addr_with_reg(base_addr, 2));
        low_u16(dword >> 16)
    }
}

#[inline]
fn low_u8(v: u32) -> u8 {
    // Masking to 8 bits then casting is lossless by construction.
    (v & 0xFF) as u8
}

#[inline]
fn low_u16(v: u32) -> u16 {
    // Masking to 16 bits then casting is lossless by construction.
    (v & 0xFFFF) as u16
}

#[inline]
fn addr_with_reg(addr: ConfigAddress, register: u8) -> ConfigAddress {
    ConfigAddress { register, ..addr }
}

#[inline]
fn addr_with_byte_offset(addr: ConfigAddress, byte_offset: u8) -> ConfigAddress {
    addr_with_reg(addr, byte_offset >> 2)
}

fn unpack_bdf(bdf: u64, register: u8) -> ConfigAddress {
    // Each field is masked to its hardware width before truncation,
    // so the `as u8` casts are lossless by construction.
    ConfigAddress {
        bus: ((bdf >> 16) & 0xFF) as u8,
        device: ((bdf >> 11) & 0x1F) as u8,
        function: ((bdf >> 8) & 0x7) as u8,
        register,
    }
}

fn decode_msi<C: ConfigSpace>(
    _this: &Pci<C>,
    _base: ConfigAddress,
    offset: u8,
    msg_ctrl: u16,
) -> Capability {
    // `mmc` is a 3-bit field; the mask + cast is lossless.
    let mmc = ((msg_ctrl >> 1) & 0x7) as u8;
    Capability::Msi {
        offset,
        message_count: 1 << mmc,
        addressing_64bit: msg_ctrl & 0x80 != 0,
    }
}

fn decode_msix<C: ConfigSpace>(
    this: &Pci<C>,
    base: ConfigAddress,
    offset: u8,
    msg_ctrl: u16,
) -> Capability {
    let table_size = (msg_ctrl & 0x7FF) + 1;
    // Table offset/BIR lives at cap_offset + 4 (dword 1 of cap).
    let table_dword = this.config.read32(addr_with_byte_offset(base, offset + 4));
    let pba_dword = this.config.read32(addr_with_byte_offset(base, offset + 8));
    Capability::MsiX {
        offset,
        table_size,
        // Mask + cast: `table_dword & 0x7` is a 3-bit field, lossless.
        table_bar: (table_dword & 0x7) as u8,
        table_offset: table_dword & 0xFFFF_FFF8,
        pba_bar: (pba_dword & 0x7) as u8,
        pba_offset: pba_dword & 0xFFFF_FFF8,
    }
}

fn decode_virtio<C: ConfigSpace>(
    this: &Pci<C>,
    base: ConfigAddress,
    offset: u8,
    msg_ctrl: u16,
) -> Capability {
    // The virtio cap header reuses the vendor-specific layout: the
    // upper half of the header dword (`msg_ctrl`) carries `cap_len`
    // in its low byte and `cfg_type` in its high byte (virtio 1.x
    // §4.1.4). Mask + cast of an 8-bit field is lossless.
    let cfg_type = (msg_ctrl >> 8) as u8;
    // `bar` is byte 4 of the capability (dword 1, low byte).
    let bar = (this.config.read32(addr_with_byte_offset(base, offset + 4)) & 0x7) as u8;
    // `offset`/`length` are dwords 2 and 3 of the capability.
    let bar_offset = this.config.read32(addr_with_byte_offset(base, offset + 8));
    let length = this.config.read32(addr_with_byte_offset(base, offset + 12));
    if cfg_type == VIRTIO_CFG_NOTIFY {
        // The notification structure appends `notify_off_multiplier`
        // as dword 4 of the capability (virtio 1.x §4.1.4.4).
        let notify_off_multiplier = this.config.read32(addr_with_byte_offset(base, offset + 16));
        Capability::VirtioNotify {
            offset,
            bar,
            bar_offset,
            length,
            notify_off_multiplier,
        }
    } else {
        Capability::Virtio {
            offset,
            cfg_type,
            bar,
            bar_offset,
            length,
        }
    }
}
