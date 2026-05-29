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
use rustos_abi::{DriverError, MmioMapError, MmioMapper, RegisterWindow};

use crate::config::{BarDescriptor, BarKind, Capability, ConfigAddress, ConfigSpace};

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
        let mut descriptors = [BarDescriptor {
            index: 0,
            kind: BarKind::Memory32,
            base: 0,
            size: 0,
            prefetchable: false,
        }; MAX_BARS];
        let n = self.bars(bdf, &mut descriptors)?;
        let bar = descriptors[..n]
            .iter()
            .find(|b| b.index == bar_index)
            .ok_or(DriverError::NotFound)?;
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
