//! PCI configuration-space addressing and descriptor types.
//!
//! The types live in their own module so the enumeration logic in
//! [`crate::enumerate`] and the mechanism-#1 PIO bridge in
//! [`crate::mech_one`] both depend on a single source of truth for
//! the on-wire layout. Nothing here is re-exported outside the
//! crate (`AGENTS.md` §8); the test module exercises every type
//! directly.
//
// `dead_code` is muted at the module level because the public surface
// of this driver crate is, by `AGENTS.md` §8, a single `register`
// function — every other item here is reached only through `dyn Bus`
// dispatch which the driver host wires up at load time and through
// the in-crate `#[cfg(test)]` module that exercises each item
// directly. Without this annotation a non-test build of the lib
// half warns about every helper.
#![allow(dead_code)]

/// A `(bus, device, function, register)` quadruple addressing one
/// 32-bit dword of PCI configuration space.
///
/// Encodes to the 32-bit value written to `0xCF8` per PCI Local Bus
/// 3.0 §3.2.2.3.2:
///
/// ```text
///  bit 31    : enable (1)
///  bits 30..24: reserved (0)
///  bits 23..16: bus
///  bits 15..11: device
///  bits 10..8 : function
///  bits 7..2  : register dword index
///  bits 1..0  : 0 (dword-aligned)
/// ```
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ConfigAddress {
    /// PCI bus number (0..=255).
    pub bus: u8,
    /// PCI device number on the bus (0..=31).
    pub device: u8,
    /// Function number within the device (0..=7).
    pub function: u8,
    /// Configuration-space register dword index (0..=63, i.e. the
    /// byte offset divided by 4 — restricted to the legacy 256-byte
    /// configuration space, sufficient for every device this driver
    /// needs to enumerate in Stage 4).
    pub register: u8,
}

impl ConfigAddress {
    /// Encoded `0xCF8` value, with the high enable bit set.
    ///
    /// Returns `None` if any field exceeds its hardware range; this
    /// is the single defensive gate for the entire driver.
    #[must_use]
    pub fn to_cf8(self) -> Option<u32> {
        if self.device > 31 || self.function > 7 || self.register > 63 {
            return None;
        }
        let bus = u32::from(self.bus);
        let dev = u32::from(self.device);
        let func = u32::from(self.function);
        // `register` carries a *dword* index, not a byte offset; the
        // `<< 2` produces the canonical byte-aligned form the
        // hardware expects.
        let reg = u32::from(self.register) << 2;
        Some(0x8000_0000 | (bus << 16) | (dev << 11) | (func << 8) | (reg & 0xFC))
    }

    /// Pack into the [`rustos_abi::driver::bus::BusDevice::address`]
    /// slot the driver hands back to the host.
    #[must_use]
    pub const fn pack_bdf(self) -> u64 {
        ((self.bus as u64) << 16) | ((self.device as u64) << 11) | ((self.function as u64) << 8)
    }
}

/// Read or write 32-bit PCI configuration dwords.
///
/// Implementations:
///
/// * [`crate::mech_one::PortIoConfigSpace`] — real hardware, x86_64
///   PIO; behind a [`crate::mech_one::PortIo`] seam so the unit
///   tests can drive it without touching the actual `in`/`out`
///   instructions.
/// * `tests::MockConfigSpace` — table-driven fixture for the
///   in-crate enumeration tests.
///
/// The trait is intentionally small: writes only exist to support
/// the BAR-sizing probe sequence (`AGENTS.md` §10 PCI conformance).
pub trait ConfigSpace {
    /// Read a 32-bit configuration dword.
    fn read32(&self, addr: ConfigAddress) -> u32;
    /// Write a 32-bit configuration dword.
    fn write32(&self, addr: ConfigAddress, value: u32);
}

/// Kind of a Base Address Register slot.
///
/// Matches PCI Local Bus 3.0 §6.2.5.1 BAR layout bits.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BarKind {
    /// 32-bit memory-mapped region (BAR bits 2..=1 == 0b00).
    Memory32,
    /// 64-bit memory-mapped region (BAR bits 2..=1 == 0b10); paired
    /// with the next-higher BAR slot carrying the upper 32 bits.
    Memory64,
    /// I/O-port region (BAR bit 0 == 1).
    Io,
}

/// One enumerated BAR slot.
///
/// The `size` field is computed by the standard write-FFFFFFFF /
/// read-back probe sequence and is the *power-of-two* span the
/// driver host must reserve when routing the BAR mapping request
/// through the kernel memory capability.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BarDescriptor {
    /// BAR index within the function's configuration space (0..=5).
    pub index: u8,
    /// BAR layout — memory-mapped (32 / 64) or I/O.
    pub kind: BarKind,
    /// Base address read from the BAR; for [`BarKind::Memory64`]
    /// this already includes the upper-32-bit slot.
    pub base: u64,
    /// Size in bytes (power of two), or `0` if the BAR is unused.
    pub size: u64,
    /// Prefetchable hint from BAR bit 3 (memory BARs only).
    pub prefetchable: bool,
}

/// One enumerated PCI capability-list entry.
///
/// MSI and MSI-X are recognised explicitly because the bus driver
/// must surface their addressing for the virtio-blk / virtio-net
/// drivers in Stage 4.D. Other capability IDs are surfaced
/// opaquely so the upper-layer driver host can audit them without
/// re-walking config space.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Capability {
    /// MSI capability (`cap_id = 0x05`).
    Msi {
        /// Byte offset of the capability header in configuration space.
        offset: u8,
        /// Number of distinct message vectors the device can request
        /// (decoded from the Message Control register's MMC field).
        message_count: u8,
        /// `true` if the capability advertises 64-bit addressing.
        addressing_64bit: bool,
    },
    /// MSI-X capability (`cap_id = 0x11`).
    MsiX {
        /// Byte offset of the capability header in configuration space.
        offset: u8,
        /// Number of entries in the MSI-X table (decoded from
        /// `table_size = (msg_ctrl & 0x7FF) + 1`).
        table_size: u16,
        /// BAR index containing the MSI-X table.
        table_bar: u8,
        /// Offset of the table within `table_bar`.
        table_offset: u32,
        /// BAR index containing the Pending Bit Array.
        pba_bar: u8,
        /// Offset of the PBA within `pba_bar`.
        pba_offset: u32,
    },
    /// Any other capability ID. Surfaced opaquely; the host audits.
    Other {
        /// Byte offset of the capability header in configuration space.
        offset: u8,
        /// Raw `cap_id` byte.
        id: u8,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf8_encodes_canonical_example() {
        // Bus 0, device 0x1F, function 0 (LPC bridge on q35), register 0.
        let addr = ConfigAddress {
            bus: 0,
            device: 0x1F,
            function: 0,
            register: 0,
        };
        // 0x8000_0000 | (0 << 16) | (0x1F << 11) | (0 << 8) | 0 == 0x8000_F800
        assert_eq!(addr.to_cf8(), Some(0x8000_F800));
    }

    #[test]
    fn cf8_rejects_out_of_range() {
        assert_eq!(
            ConfigAddress {
                bus: 0,
                device: 32,
                function: 0,
                register: 0
            }
            .to_cf8(),
            None,
        );
        assert_eq!(
            ConfigAddress {
                bus: 0,
                device: 0,
                function: 8,
                register: 0
            }
            .to_cf8(),
            None,
        );
        assert_eq!(
            ConfigAddress {
                bus: 0,
                device: 0,
                function: 0,
                register: 64
            }
            .to_cf8(),
            None,
        );
    }

    #[test]
    fn pack_bdf_matches_bit_layout() {
        let addr = ConfigAddress {
            bus: 0x12,
            device: 0x0A,
            function: 0x3,
            register: 0,
        };
        assert_eq!(addr.pack_bdf(), (0x12 << 16) | (0x0A << 11) | (0x3 << 8));
    }
}
