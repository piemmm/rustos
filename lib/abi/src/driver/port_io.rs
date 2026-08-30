//! Legacy port-I/O access seam (`abi-v1`).
//!
//! A [`PortIo`] is the host↔driver ABI seam through which a bus driver
//! that depends on architectural port I/O — today only `lib/pci`
//! reaching PCI configuration space through mechanism #1 (PCI Local Bus
//! 3.0 §3.2.2.3.2: the address word at I/O port `0xCF8` and the data word
//! at `0xCFC`) — issues 32-bit reads and writes against an I/O port.
//!
//! It lives in `lib/abi` for the same reason [`MmioMapper`](super::MmioMapper)
//! does: the bus driver has to be able to name the seam without pulling in
//! an architecture port (`kernel/arch/<target>`), which would invert the
//! dependency direction and gate the driver on a target-conditional
//! `cfg` — both forbidden by the charter's layering rules. The architecture
//! port (for x86_64, `kernel/arch/x86_64`) supplies the only real
//! implementation, encapsulating the `in`/`out` instructions and their
//! `unsafe` invariants behind this safe trait. The
//! driver never issues a port-I/O instruction itself and carries no
//! architecture gate.
//!
//! Port I/O is an x86-family mechanism; architectures without an I/O port
//! space (aarch64, riscv64, wasm32) simply never construct a [`PortIo`]
//! and reach `PCIe` through memory-mapped ECAM instead — a separate seam.

/// 32-bit port-I/O access seam.
///
/// The trait is *not* `unsafe` to implement: an implementor is
/// responsible for whatever invariants its backing transport requires,
/// and the only in-tree implementor (the x86_64 architecture port)
/// encapsulates the single `unsafe` block behind these two safe methods.
/// A driver consumes a `PortIo` by value or behind `&dyn PortIo` and only
/// ever issues the documented PCI configuration-port accesses through it.
pub trait PortIo {
    /// Read 32 bits from I/O port `port`.
    fn read32(&self, port: u16) -> u32;

    /// Write 32 bits of `value` to I/O port `port`.
    fn write32(&self, port: u16, value: u32);
}

/// 8-bit port-I/O access seam.
///
/// Distinct from [`PortIo`] (which is 32-bit and exists solely for PCI
/// mechanism #1 configuration access): legacy register files such as the
/// Intel 8042 keyboard/mouse controller (`drivers/input/ps2`, status/command
/// port `0x64` and data port `0x60`) and the 16550 UART are byte-addressed,
/// and a 32-bit read would alias three neighbouring registers. `PortIo` is a
/// frozen `abi-v1` interface, so the byte width ships as
/// this separate versioned trait rather than as an added method.
///
/// It lives in `lib/abi` for the same reason [`PortIo`] does: a driver must
/// name the seam without depending on an architecture port
/// (`kernel/arch/<target>`), which would invert the dependency direction and
/// force a target-conditional `cfg` on the driver — both forbidden by the
/// charter's layering rules. The architecture port (for x86_64,
/// `kernel/arch/x86_64`) supplies the only real implementation, encapsulating
/// the `inb`/`outb` instructions and their `unsafe` invariants behind this
/// safe trait. The driver never issues a port-I/O
/// instruction itself and carries no architecture gate.
///
/// Port I/O is an x86-family mechanism; architectures without an I/O port
/// space (aarch64, riscv64, wasm32) simply never construct a [`PortIo8`].
pub trait PortIo8 {
    /// Read 8 bits from I/O port `port`.
    fn read8(&self, port: u16) -> u8;

    /// Write 8 bits of `value` to I/O port `port`.
    fn write8(&self, port: u16, value: u8);
}

/// Access width of one port-I/O transfer.
///
/// The x86 I/O port space is byte-addressed and the `in`/`out` instructions
/// come in three widths, so a transfer names both a port and how many bytes
/// of it are moved. Carried across the user↔kernel boundary by the
/// capability-gated port-I/O traps, which bound `port .. port + bytes()`
/// inside the caller's granted port range: a wider access than the grant
/// covers reaches a neighbouring device's registers, so the width is part of
/// what the kernel validates, not a hint.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[repr(u8)]
pub enum PortWidth {
    /// One byte (`inb` / `outb`).
    Byte = 1,
    /// Two bytes (`inw` / `outw`).
    Word = 2,
    /// Four bytes (`inl` / `outl`).
    Dword = 4,
}

impl PortWidth {
    /// Bytes of I/O port space one transfer of this width covers.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self as u64
    }

    /// Decode the wire value, refusing anything that is not one of the three
    /// architectural widths (fail closed — never a widened access).
    ///
    /// # Errors
    ///
    /// [`crate::Errno::OutOfRange`] for any other value.
    pub const fn from_u8(raw: u8) -> Result<Self, crate::Errno> {
        match raw {
            1 => Ok(Self::Byte),
            2 => Ok(Self::Word),
            4 => Ok(Self::Dword),
            _ => Err(crate::Errno::OutOfRange),
        }
    }

    /// Narrow a caller's word to this width, keeping only the bits the
    /// transfer can carry.
    ///
    /// A caller that supplied a wider value is naming bits the instruction
    /// cannot move, so they are dropped here rather than reaching the bus —
    /// and the result cannot then disagree with the width it was cut to.
    #[must_use]
    pub const fn narrow(self, value: u64) -> PortValue {
        // Cut through the little-endian byte image rather than a narrowing
        // cast: the split is then total by construction, with no arm that
        // could fail and nothing to fabricate if it did.
        let b = value.to_le_bytes();
        match self {
            Self::Byte => PortValue::Byte(b[0]),
            Self::Word => PortValue::Word(u16::from_le_bytes([b[0], b[1]])),
            Self::Dword => PortValue::Dword(u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        }
    }
}

/// One port-I/O transfer's value, carrying its own width.
///
/// A width and a loose integer can disagree; this cannot. The kernel narrows
/// a caller's word to the granted transfer's width once, when it validates
/// the request, so the mechanism below it receives a value that is already
/// exactly as wide as the instruction it will issue.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PortValue {
    /// One byte (`inb` / `outb`).
    Byte(u8),
    /// Two bytes (`inw` / `outw`).
    Word(u16),
    /// Four bytes (`inl` / `outl`).
    Dword(u32),
}

impl PortValue {
    /// The value zero-extended to the widest transfer, for returning across
    /// the register boundary.
    #[must_use]
    pub fn as_u32(self) -> u32 {
        match self {
            Self::Byte(v) => u32::from(v),
            Self::Word(v) => u32::from(v),
            Self::Dword(v) => v,
        }
    }

    /// This value's width.
    #[must_use]
    pub const fn width(self) -> PortWidth {
        match self {
            Self::Byte(_) => PortWidth::Byte,
            Self::Word(_) => PortWidth::Word,
            Self::Dword(_) => PortWidth::Dword,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PortIo, PortIo8, PortValue, PortWidth};
    use core::cell::Cell;

    /// Records the last `(port, value)` written and replies to reads
    /// with `port`'s low byte, proving both seams are object-safe and
    /// usable behind `&dyn`.
    struct Recorder {
        last_port: Cell<u16>,
        last_value: Cell<u32>,
    }

    impl PortIo for Recorder {
        fn read32(&self, port: u16) -> u32 {
            self.last_port.set(port);
            u32::from(port)
        }
        fn write32(&self, port: u16, value: u32) {
            self.last_port.set(port);
            self.last_value.set(value);
        }
    }

    impl PortIo8 for Recorder {
        fn read8(&self, port: u16) -> u8 {
            self.last_port.set(port);
            port.to_le_bytes()[0]
        }
        fn write8(&self, port: u16, value: u8) {
            self.last_port.set(port);
            self.last_value.set(u32::from(value));
        }
    }

    #[test]
    fn port_io8_is_object_safe_and_byte_wide() {
        let rec = Recorder {
            last_port: Cell::new(0),
            last_value: Cell::new(0),
        };
        let io: &dyn PortIo8 = &rec;
        io.write8(0x64, 0xAE);
        assert_eq!(rec.last_port.get(), 0x64);
        assert_eq!(rec.last_value.get(), 0xAE);
        assert_eq!(io.read8(0x60), 0x60);
    }

    #[test]
    fn port_io_32_and_8_are_independent_seams() {
        let rec = Recorder {
            last_port: Cell::new(0),
            last_value: Cell::new(0),
        };
        let wide: &dyn PortIo = &rec;
        assert_eq!(wide.read32(0x00CF), 0x00CF);
        let narrow: &dyn PortIo8 = &rec;
        assert_eq!(narrow.read8(0x00CF), 0xCF);
    }

    #[test]
    fn a_width_covers_its_own_byte_count_and_masks_its_own_value() {
        assert_eq!(PortWidth::Byte.bytes(), 1);
        assert_eq!(PortWidth::Word.bytes(), 2);
        assert_eq!(PortWidth::Dword.bytes(), 4);
    }

    #[test]
    fn narrowing_keeps_only_the_bits_the_transfer_carries() {
        let wide = 0xdead_beef_1234_5678u64;
        assert_eq!(PortWidth::Byte.narrow(wide), PortValue::Byte(0x78));
        assert_eq!(PortWidth::Word.narrow(wide), PortValue::Word(0x5678));
        assert_eq!(PortWidth::Dword.narrow(wide), PortValue::Dword(0x1234_5678));
    }

    #[test]
    fn a_value_reports_the_width_it_was_cut_to() {
        assert_eq!(PortValue::Byte(1).width(), PortWidth::Byte);
        assert_eq!(PortValue::Word(1).width(), PortWidth::Word);
        assert_eq!(PortValue::Dword(1).width(), PortWidth::Dword);
        assert_eq!(PortValue::Byte(0xff).as_u32(), 0xff);
        assert_eq!(PortValue::Dword(u32::MAX).as_u32(), u32::MAX);
    }

    #[test]
    fn only_the_three_architectural_widths_decode() {
        assert_eq!(PortWidth::from_u8(1), Ok(PortWidth::Byte));
        assert_eq!(PortWidth::from_u8(2), Ok(PortWidth::Word));
        assert_eq!(PortWidth::from_u8(4), Ok(PortWidth::Dword));
        for raw in [0u8, 3, 5, 8, 16, 255] {
            assert_eq!(PortWidth::from_u8(raw), Err(crate::Errno::OutOfRange));
        }
    }
}
