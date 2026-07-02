//! Legacy 8259A PIC quiesce: remap away from the exception vectors and
//! mask every line.
//!
//! RustOS drives interrupts exclusively through the LAPIC/IO-APIC
//! (`apic`, `irq`), so the two legacy 8259A controllers must never
//! deliver anything. Firmware cannot be relied on to leave them quiet:
//! `SeaBIOS` — the BIOS in front of QEMU's PVH `-kernel` direct boot, and
//! the BIOS on real legacy-boot hardware — hands over with the PICs
//! **unmasked at their power-on vector base 8**, so the first PIT tick
//! taken with `IF=1` lands on IDT vector 8, which in long mode is the
//! `#DF` double-fault gate. (OVMF happens to mask the PICs, which is why
//! the UEFI boot path never exposed this.)
//!
//! [`remap_and_mask_all`] runs once on the BSP from the boot stub
//! (`entry.rs`), before any kernel code enables interrupts: it
//! re-initialises both controllers (Intel 8259A datasheet, ICW1..ICW4)
//! with their vector bases moved to [`MASTER_VECTOR_BASE`] /
//! [`SLAVE_VECTOR_BASE`] — clear of the architectural exception range —
//! and then masks all sixteen lines. The remap is defence in depth: with
//! every line masked nothing should ever be delivered, but if a line
//! were ever unmasked by mistake it would land on an ordinary, unclaimed
//! vector rather than being decoded as an exception.

use rustos_abi::driver::port_io::PortIo8;

/// Master 8259A command port.
const MASTER_CMD: u16 = 0x20;
/// Master 8259A data port.
const MASTER_DATA: u16 = 0x21;
/// Slave 8259A command port.
const SLAVE_CMD: u16 = 0xA0;
/// Slave 8259A data port.
const SLAVE_DATA: u16 = 0xA1;

/// Vector base the master PIC is remapped to (IRQ0..7 → 0x20..0x27).
/// Sits above the architectural exception range (vectors 0..31 are
/// reserved by the ISA for exceptions, Intel SDM Vol 3 §6.2) and below
/// the IO-APIC external-IRQ range (`irq`, 0x30..=0xFE).
pub const MASTER_VECTOR_BASE: u8 = 0x20;

/// Vector base the slave PIC is remapped to (IRQ8..15 → 0x28..0x2F).
pub const SLAVE_VECTOR_BASE: u8 = 0x28;

// The remap exists to keep a stray legacy IRQ off the architectural
// exception vectors (0..31) and out of the IO-APIC external-IRQ
// allocation (`irq`, 0x30..=0xFE) — enforced at compile time.
const _: () = assert!(MASTER_VECTOR_BASE >= 32 && MASTER_VECTOR_BASE + 7 < 0x30);
const _: () = assert!(SLAVE_VECTOR_BASE >= 32 && SLAVE_VECTOR_BASE + 7 < 0x30);

/// Re-initialise both 8259As with remapped vector bases and mask every
/// line. Idempotent; called once on the BSP before interrupts are ever
/// enabled.
pub fn remap_and_mask_all(io: &dyn PortIo8) {
    // ICW1: edge-triggered, cascade mode, ICW4 present.
    io.write8(MASTER_CMD, 0x11);
    io.write8(SLAVE_CMD, 0x11);
    // ICW2: vector bases.
    io.write8(MASTER_DATA, MASTER_VECTOR_BASE);
    io.write8(SLAVE_DATA, SLAVE_VECTOR_BASE);
    // ICW3: slave on master line 2; slave cascade identity 2.
    io.write8(MASTER_DATA, 0x04);
    io.write8(SLAVE_DATA, 0x02);
    // ICW4: 8086 mode.
    io.write8(MASTER_DATA, 0x01);
    io.write8(SLAVE_DATA, 0x01);
    // OCW1: mask all eight lines on each controller.
    io.write8(MASTER_DATA, 0xFF);
    io.write8(SLAVE_DATA, 0xFF);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;

    /// Records every 8-bit port write in order (the init sequence is
    /// exactly ten writes; an eleventh is itself a failure).
    struct RecordingIo {
        writes: RefCell<([(u16, u8); 10], usize)>,
    }

    impl PortIo8 for RecordingIo {
        fn read8(&self, _port: u16) -> u8 {
            0
        }
        fn write8(&self, port: u16, value: u8) {
            let mut guard = self.writes.borrow_mut();
            let index = guard.1;
            assert!(index < guard.0.len(), "unexpected extra PIC write");
            guard.0[index] = (port, value);
            guard.1 = index + 1;
        }
    }

    #[test]
    fn init_sequence_remaps_and_masks_both_controllers() {
        let io = RecordingIo {
            writes: RefCell::new(([(0, 0); 10], 0)),
        };
        remap_and_mask_all(&io);
        let (writes, count) = *io.writes.borrow();
        assert_eq!(count, 10);
        assert_eq!(
            writes,
            [
                (MASTER_CMD, 0x11),
                (SLAVE_CMD, 0x11),
                (MASTER_DATA, MASTER_VECTOR_BASE),
                (SLAVE_DATA, SLAVE_VECTOR_BASE),
                (MASTER_DATA, 0x04),
                (SLAVE_DATA, 0x02),
                (MASTER_DATA, 0x01),
                (SLAVE_DATA, 0x01),
                (MASTER_DATA, 0xFF),
                (SLAVE_DATA, 0xFF),
            ]
        );
    }
}
