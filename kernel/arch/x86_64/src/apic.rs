//! LAPIC and IO-APIC drivers.
//!
//! Both controllers are programmed through memory-mapped 32-bit
//! registers. This module abstracts those reads/writes behind two
//! traits — [`LapicMmio`] and [`IoApicMmio`] — so the entire control
//! flow is exercised by host-side unit tests against an in-memory
//! mock. The bare-metal implementation [`VolatileLapicMmio`] /
//! [`VolatileIoApicMmio`] is compiled only on `target_os = "none"`
//! and wraps `core::ptr::{read,write}_volatile` with a single
//! documented `// SAFETY:` block.
//!
//! Scope of this Stage-3a (a) commit:
//!
//! * Software-enable the LAPIC and program the Spurious Interrupt
//!   Vector Register.
//! * EOI primitive used by every interrupt handler.
//! * INIT / SIPI IPI sequence (consumed in Stage-3a (b) for AP
//!   bring-up — verified here by the mock recording the writes).
//! * IO-APIC version / max-entry decoding and per-line redirection-
//!   entry programming (legacy ISA IRQs default to identity-mapped
//!   with active-high + edge polarity).
//!
//! Scheduler-preemption wiring lives in `apic_timer.rs`; the
//! interrupt prologue that calls `Lapic::eoi` lives in Stage-3a (c).
//!
//! References:
//! * Intel SDM Volume 3A, Chapter 11 ("Advanced Programmable
//!   Interrupt Controller (APIC)").
//! * Intel 82093AA I/O Advanced Programmable Interrupt Controller
//!   data sheet (the "IO-APIC" reference).

/// LAPIC MMIO offsets (Intel SDM Vol. 3A §11.4.1, Table 11-1).
mod lapic_reg {
    pub const ID: usize = 0x020;
    pub const VERSION: usize = 0x030;
    pub const TPR: usize = 0x080;
    pub const EOI: usize = 0x0B0;
    pub const LDR: usize = 0x0D0;
    pub const DFR: usize = 0x0E0;
    pub const SPURIOUS: usize = 0x0F0;
    pub const ICR_LOW: usize = 0x300;
    pub const ICR_HIGH: usize = 0x310;
    pub const TIMER_LVT: usize = 0x320;
    pub const TIMER_INITIAL_COUNT: usize = 0x380;
    pub const TIMER_CURRENT_COUNT: usize = 0x390;
    pub const TIMER_DIVIDE_CONFIG: usize = 0x3E0;
}

/// LAPIC Spurious Interrupt Vector Register bits (SDM §11.9).
pub mod spurious {
    /// Software-enable bit. Must be set for the LAPIC to deliver any
    /// interrupt; cleared by INIT.
    pub const ENABLE: u32 = 1 << 8;
}

/// Delivery modes for `ICR_LOW.delivery_mode` (SDM §11.6.1, Table 11-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    /// Standard fixed-vector delivery.
    Fixed = 0b000,
    /// SMI — `vector` must be zero.
    Smi = 0b010,
    /// NMI — `vector` is ignored.
    Nmi = 0b100,
    /// INIT IPI; clears APIC state on target.
    Init = 0b101,
    /// Start-Up IPI; vector holds physical frame `vector * 0x1000`.
    StartUp = 0b110,
}

/// Trait wrapping volatile 32-bit MMIO reads/writes to a LAPIC.
///
/// The production implementation is [`VolatileLapicMmio`]; tests use
/// an in-memory mock (see the `tests_support` module, `#[cfg(test)]`-only).
pub trait LapicMmio {
    /// Read the 32-bit register at byte offset `offset`.
    ///
    /// # Safety
    ///
    /// `offset` must be a 16-byte-aligned offset within the 4 KiB
    /// LAPIC MMIO window (offsets 0..=0xFF0). Callers in this module
    /// only pass constants from `lapic_reg`; implementors may
    /// otherwise assume the offset is valid.
    fn read(&self, offset: usize) -> u32;

    /// Write `value` to the 32-bit register at byte offset `offset`.
    ///
    /// # Safety
    ///
    /// Same offset constraints as [`Self::read`].
    fn write(&mut self, offset: usize, value: u32);
}

/// LAPIC driver (per-CPU). Holds a handle to the MMIO accessor only;
/// no other state — the LAPIC itself is the source of truth.
#[derive(Debug)]
pub struct Lapic<M: LapicMmio> {
    mmio: M,
}

impl<M: LapicMmio> Lapic<M> {
    /// Construct over a previously-mapped MMIO window.
    pub const fn new(mmio: M) -> Self {
        Self { mmio }
    }

    /// The CPU's LAPIC ID (matches the MADT `LocalApic.apic_id`).
    pub fn id(&self) -> u8 {
        ((self.mmio.read(lapic_reg::ID) >> 24) & 0xFF) as u8
    }

    /// LAPIC version (bottom byte of the VERSION register).
    pub fn version(&self) -> u8 {
        (self.mmio.read(lapic_reg::VERSION) & 0xFF) as u8
    }

    /// Software-enable the LAPIC and program its spurious-interrupt
    /// vector. Per Intel SDM §11.9 this must be the first write done
    /// to the LAPIC after the firmware hand-off, before any other
    /// interrupt is unmasked.
    pub fn software_enable(&mut self, spurious_vector: u8) {
        // TPR = 0: accept every priority.
        self.mmio.write(lapic_reg::TPR, 0);
        // Logical destination mode = flat (DFR all-ones).
        self.mmio.write(lapic_reg::DFR, 0xFFFF_FFFF);
        self.mmio.write(lapic_reg::LDR, 0x0100_0000); // logical id = 1
        let svr = spurious::ENABLE | u32::from(spurious_vector);
        self.mmio.write(lapic_reg::SPURIOUS, svr);
    }

    /// End-of-interrupt: every interrupt handler must call this exactly
    /// once before `iretq`. Writing any value clears the in-service bit.
    pub fn eoi(&mut self) {
        self.mmio.write(lapic_reg::EOI, 0);
    }

    /// Send an IPI to `target_apic_id` with the requested delivery mode
    /// and vector. The write order is `ICR_HIGH` first, then `ICR_LOW`;
    /// the latter is the "fire" register.
    pub fn send_ipi(&mut self, target_apic_id: u8, mode: DeliveryMode, vector: u8) {
        self.mmio
            .write(lapic_reg::ICR_HIGH, u32::from(target_apic_id) << 24);
        let icr_low = u32::from(vector)
            | ((mode as u32) << 8)
            // 1 << 14 = Level Assert (required for non-INIT IPIs;
            // harmless for INIT Assert).
            | (1 << 14);
        self.mmio.write(lapic_reg::ICR_LOW, icr_low);
    }

    /// Issue the INIT-deassert IPI used between INIT and SIPI during
    /// AP bring-up. Caller is responsible for the spec-mandated delays.
    pub fn send_init_deassert(&mut self, target_apic_id: u8) {
        self.mmio
            .write(lapic_reg::ICR_HIGH, u32::from(target_apic_id) << 24);
        // Mode = INIT, Level = de-assert (bit 14 = 0), Trigger = level
        // (bit 15 = 1).
        let icr_low = ((DeliveryMode::Init as u32) << 8) | (1 << 15);
        self.mmio.write(lapic_reg::ICR_LOW, icr_low);
    }

    /// Borrow the underlying MMIO accessor; used by `apic_timer`.
    pub fn mmio_mut(&mut self) -> &mut M {
        &mut self.mmio
    }

    /// LAPIC timer LVT register offset, exposed for `apic_timer`.
    pub const TIMER_LVT_OFFSET: usize = lapic_reg::TIMER_LVT;
    /// LAPIC timer initial-count register offset.
    pub const TIMER_INITIAL_COUNT_OFFSET: usize = lapic_reg::TIMER_INITIAL_COUNT;
    /// LAPIC timer current-count register offset.
    pub const TIMER_CURRENT_COUNT_OFFSET: usize = lapic_reg::TIMER_CURRENT_COUNT;
    /// LAPIC timer divide-configuration register offset.
    pub const TIMER_DIVIDE_CONFIG_OFFSET: usize = lapic_reg::TIMER_DIVIDE_CONFIG;
}

// --- Bare-metal MMIO impl --------------------------------------------

/// Volatile-MMIO impl of [`LapicMmio`] for a real LAPIC.
///
/// Construct with [`VolatileLapicMmio::new`] passing the kernel-mapped
/// virtual address of the LAPIC's 4 KiB MMIO window.
#[cfg(any(target_os = "none", doc))]
#[derive(Debug)]
pub struct VolatileLapicMmio {
    base: *mut u32,
}

#[cfg(any(target_os = "none", doc))]
// SAFETY: VolatileLapicMmio holds a pointer to per-CPU MMIO that is
// only ever accessed through volatile reads/writes; the LAPIC window is
// per-CPU and the kernel never aliases it across threads. Marking
// `Send` lets the driver be moved across the AP bring-up boundary.
unsafe impl Send for VolatileLapicMmio {}

#[cfg(any(target_os = "none", doc))]
impl VolatileLapicMmio {
    /// Wrap an existing kernel-mapped LAPIC base address.
    ///
    /// # Safety
    ///
    /// `base` must be a valid kernel-mapped virtual address of the
    /// LAPIC's 4 KiB MMIO window, aliased nowhere else.
    pub const unsafe fn new(base: *mut u32) -> Self {
        Self { base }
    }
}

#[cfg(any(target_os = "none", doc))]
impl LapicMmio for VolatileLapicMmio {
    fn read(&self, offset: usize) -> u32 {
        // SAFETY: offset is a documented LAPIC register offset within
        // the 4 KiB window the constructor's safety contract covers.
        unsafe { core::ptr::read_volatile(self.base.byte_add(offset)) }
    }
    fn write(&mut self, offset: usize, value: u32) {
        // SAFETY: as for `read`.
        unsafe { core::ptr::write_volatile(self.base.byte_add(offset), value) }
    }
}

// --- IO-APIC ---------------------------------------------------------

/// Trait wrapping the two IO-APIC MMIO ports (`IOREGSEL` at +0x00 and
/// `IOWIN` at +0x10).
pub trait IoApicMmio {
    /// Read the 32-bit indirect register `reg`.
    fn read(&mut self, reg: u8) -> u32;
    /// Write `value` to the 32-bit indirect register `reg`.
    fn write(&mut self, reg: u8, value: u32);
}

/// IO-APIC driver.
#[derive(Debug)]
pub struct IoApic<M: IoApicMmio> {
    mmio: M,
}

impl<M: IoApicMmio> IoApic<M> {
    /// Construct over a previously-mapped MMIO window.
    pub const fn new(mmio: M) -> Self {
        Self { mmio }
    }

    /// Decoded ID (bits 24..28 of the `IOAPICID` register).
    pub fn id(&mut self) -> u8 {
        ((self.mmio.read(0x00) >> 24) & 0x0F) as u8
    }

    /// Maximum redirection entry index (highest valid IRQ on this
    /// controller is `max_redirection_entry()`).
    pub fn max_redirection_entry(&mut self) -> u8 {
        ((self.mmio.read(0x01) >> 16) & 0xFF) as u8
    }

    /// Program one redirection entry.
    ///
    /// `pin` is the IO-APIC input line (0..= `max_redirection_entry`).
    /// `vector` is the CPU vector to deliver; `dest_apic_id` selects
    /// the target LAPIC. `masked` controls whether the line starts
    /// asserted-but-blocked.
    pub fn set_redirection_entry(&mut self, pin: u8, vector: u8, dest_apic_id: u8, masked: bool) {
        let mask_bit = if masked { 1u32 << 16 } else { 0 };
        let low: u32 = u32::from(vector) | mask_bit;
        let high: u32 = u32::from(dest_apic_id) << 24;
        let reg = 0x10u8 + pin.saturating_mul(2);
        self.mmio.write(reg, low);
        self.mmio.write(reg.saturating_add(1), high);
    }

    /// Read the low half of redirection entry `pin` through the
    /// underlying [`IoApicMmio`].
    ///
    /// Bit 16 is the mask bit; bits 0..7 carry the vector. Used by
    /// the kernel-binary `IoApicController::read_pin_low` accessor
    /// (Stage 4.D Item 2-tail.2 QEMU validation) to re-read the
    /// hardware mask state after `IrqTable::fire`; that path is
    /// the evidence trail for the mask-before-wake invariant
    /// documented in `docs/src/security/irq.md`.
    pub fn read_redirection_entry_low(&mut self, pin: u8) -> u32 {
        let reg = 0x10u8 + pin.saturating_mul(2);
        self.mmio.read(reg)
    }
}

/// Volatile-MMIO impl of [`IoApicMmio`] for a real IO-APIC.
#[cfg(any(target_os = "none", doc))]
#[derive(Debug)]
pub struct VolatileIoApicMmio {
    base: *mut u32,
}

#[cfg(any(target_os = "none", doc))]
// SAFETY: see `VolatileLapicMmio`.
unsafe impl Send for VolatileIoApicMmio {}

#[cfg(any(target_os = "none", doc))]
impl VolatileIoApicMmio {
    /// Wrap an existing kernel-mapped IO-APIC base address.
    ///
    /// # Safety
    ///
    /// `base` must be a valid kernel-mapped virtual address of the
    /// IO-APIC's MMIO window (typically `0xFEC0_0000` physical).
    pub const unsafe fn new(base: *mut u32) -> Self {
        Self { base }
    }
}

#[cfg(any(target_os = "none", doc))]
impl IoApicMmio for VolatileIoApicMmio {
    fn read(&mut self, reg: u8) -> u32 {
        // SAFETY: IOREGSEL and IOWIN are at fixed +0x00 and +0x10
        // offsets within the IO-APIC's MMIO window; the constructor's
        // contract covers this.
        unsafe {
            core::ptr::write_volatile(self.base, u32::from(reg));
            core::ptr::read_volatile(self.base.byte_add(0x10))
        }
    }
    fn write(&mut self, reg: u8, value: u32) {
        // SAFETY: as for `read`.
        unsafe {
            core::ptr::write_volatile(self.base, u32::from(reg));
            core::ptr::write_volatile(self.base.byte_add(0x10), value);
        }
    }
}

// --- Test support (shared with `apic_timer::tests`) ------------------
//
// `tests_support` is `#[cfg(test)]`-only and `pub(crate)`. It exists so
// the LAPIC mock is defined exactly once and reused by `apic_timer`
// (no duplication).

#[cfg(test)]
pub(crate) mod tests_support {
    extern crate std;
    use super::{IoApicMmio, LapicMmio};
    use std::collections::HashMap;
    use std::vec::Vec;

    /// Mock LAPIC backing store: a `HashMap` keyed by register offset
    /// plus a write log so tests can assert the order of operations.
    #[derive(Default)]
    pub struct MockLapicMmio {
        pub regs: HashMap<usize, u32>,
        pub writes: Vec<(usize, u32)>,
    }
    impl LapicMmio for MockLapicMmio {
        fn read(&self, off: usize) -> u32 {
            *self.regs.get(&off).unwrap_or(&0)
        }
        fn write(&mut self, off: usize, val: u32) {
            self.regs.insert(off, val);
            self.writes.push((off, val));
        }
    }

    #[derive(Default)]
    pub struct MockIoApicMmio {
        pub regs: HashMap<u8, u32>,
        pub writes: Vec<(u8, u32)>,
    }
    impl IoApicMmio for MockIoApicMmio {
        fn read(&mut self, reg: u8) -> u32 {
            *self.regs.get(&reg).unwrap_or(&0)
        }
        fn write(&mut self, reg: u8, val: u32) {
            self.regs.insert(reg, val);
            self.writes.push((reg, val));
        }
    }
}

// --- Tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::tests_support::{MockIoApicMmio, MockLapicMmio};
    use super::*;

    #[test]
    fn lapic_id_decodes_high_byte() {
        let mut mock = MockLapicMmio::default();
        mock.regs.insert(lapic_reg::ID, 0x07 << 24);
        let lapic = Lapic::new(mock);
        assert_eq!(lapic.id(), 7);
    }

    #[test]
    fn software_enable_writes_canonical_sequence() {
        let mut lapic = Lapic::new(MockLapicMmio::default());
        lapic.software_enable(0xFF);
        let writes = &lapic.mmio.writes;
        // TPR cleared, DFR=all-ones, LDR set, SVR = enable | vector.
        assert_eq!(writes[0], (lapic_reg::TPR, 0));
        assert_eq!(writes[1], (lapic_reg::DFR, 0xFFFF_FFFF));
        assert_eq!(writes[2], (lapic_reg::LDR, 0x0100_0000));
        assert_eq!(writes[3], (lapic_reg::SPURIOUS, spurious::ENABLE | 0xFF),);
    }

    #[test]
    fn eoi_writes_zero() {
        let mut lapic = Lapic::new(MockLapicMmio::default());
        lapic.eoi();
        assert_eq!(lapic.mmio.writes, [(lapic_reg::EOI, 0)]);
    }

    #[test]
    fn send_ipi_writes_high_before_low() {
        let mut lapic = Lapic::new(MockLapicMmio::default());
        lapic.send_ipi(0x03, DeliveryMode::Fixed, 0x20);
        let w = &lapic.mmio.writes;
        assert_eq!(w[0], (lapic_reg::ICR_HIGH, 0x03 << 24));
        // Vector 0x20, mode Fixed (0), level-assert bit set.
        assert_eq!(w[1], (lapic_reg::ICR_LOW, 0x20 | (1 << 14)));
    }

    #[test]
    fn init_sipi_sequence_uses_correct_modes() {
        let mut lapic = Lapic::new(MockLapicMmio::default());
        lapic.send_ipi(0x01, DeliveryMode::Init, 0);
        lapic.send_init_deassert(0x01);
        // SIPI: vector encodes physical frame; for frame 0x8000 that's 8.
        lapic.send_ipi(0x01, DeliveryMode::StartUp, 0x08);

        let writes = &lapic.mmio.writes;
        // Six writes total: high+low for each of the three IPIs.
        assert_eq!(writes.len(), 6);

        // Each high write must target apic 0x01 in bits 24..32.
        for (i, (off, val)) in writes.iter().enumerate() {
            if i % 2 == 0 {
                assert_eq!(*off, lapic_reg::ICR_HIGH);
                assert_eq!(*val >> 24, 0x01);
            }
        }

        // INIT IPI low: mode=5 (Init) in bits 8..11, assert.
        assert_eq!(writes[1].1 & 0x0700, (DeliveryMode::Init as u32) << 8);
        // SIPI low: mode=6, vector=0x08.
        assert_eq!(writes[5].1 & 0xFF, 0x08);
        assert_eq!(writes[5].1 & 0x0700, (DeliveryMode::StartUp as u32) << 8);
    }

    #[test]
    fn ioapic_decodes_id_and_max_entry() {
        let mut mock = MockIoApicMmio::default();
        mock.regs.insert(0x00, 0x0F << 24);
        mock.regs.insert(0x01, 23 << 16);
        let mut io = IoApic::new(mock);
        assert_eq!(io.id(), 0x0F);
        assert_eq!(io.max_redirection_entry(), 23);
    }

    #[test]
    fn ioapic_redirection_entry_writes_low_then_high() {
        let mut io = IoApic::new(MockIoApicMmio::default());
        io.set_redirection_entry(1, 0x21, 0x02, false);
        let w = &io.mmio.writes;
        // Pin 1 -> regs 0x12 (low) and 0x13 (high).
        assert_eq!(w[0], (0x12, 0x21));
        assert_eq!(w[1], (0x13, 0x02 << 24));
    }

    #[test]
    fn ioapic_masked_sets_mask_bit() {
        let mut io = IoApic::new(MockIoApicMmio::default());
        io.set_redirection_entry(0, 0x30, 0, true);
        let (_, low) = io.mmio.writes[0];
        assert_eq!(low & (1 << 16), 1 << 16);
    }
}
