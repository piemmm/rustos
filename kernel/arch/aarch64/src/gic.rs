//! GICv2 (ARM Generic Interrupt Controller) driver for the QEMU `virt`
//! board.
//!
//! The `virt` board's default interrupt controller is a GICv2 with the
//! distributor at [`GICD_BASE`] and the per-CPU interface at
//! [`GICC_BASE`]. This module owns the minimum surface the aarch64
//! Stage-3 primitives need:
//!
//! * `init` — enable the distributor and this CPU's interface, open the
//!   priority mask, and route interrupts to the IRQ exception.
//! * `enable_ppi` — enable one private-peripheral interrupt (the EL1
//!   physical-timer PPI, [`crate::preempt::TIMER_PPI`], is the one the
//!   timer path arms).
//! * `acknowledge` / `end_of_interrupt` — the IAR/EOIR handshake the
//!   IRQ handler runs per interrupt.
//! * `send_sgi` — raise a software-generated interrupt on a target CPU
//!   (the GICv2 mechanism behind [`crate::kernel_arch::Aarch64Arch`]'s
//!   `send_ipi`).
//!
//! # Interrupt-id classes (GICv2 spec §2.2.1)
//!
//! INTIDs `0..16` are SGIs (inter-processor), `16..32` are PPIs
//! (per-CPU peripherals — the timers live here), and `32..1020` are SPIs
//! (shared peripherals). Enabling and priority for SGIs/PPIs is
//! per-CPU through the banked first `GICD_ISENABLER` / `GICD_IPRIORITYR`
//! registers.
//!
//! # Host testability
//!
//! The register-offset math and the `GICD_SGIR` word encoding are pure
//! functions, unit-tested on the host; the MMIO reads/writes are gated
//! to the freestanding aarch64 target.

use rustos_arch_api::CpuId;

/// MMIO base of the GICv2 distributor on the `virt` board.
pub const GICD_BASE: usize = 0x0800_0000;

/// MMIO base of the GICv2 CPU interface on the `virt` board.
pub const GICC_BASE: usize = 0x0801_0000;

/// `GICD_CTLR` — distributor control (offset 0x000). Bit 0 enables
/// forwarding of pending interrupts to the CPU interfaces.
const GICD_CTLR: usize = 0x000;
/// `GICD_ISENABLER<n>` — set-enable, one bit per interrupt (base 0x100).
const GICD_ISENABLER: usize = 0x100;
/// `GICD_ICENABLER<n>` — clear-enable, one bit per interrupt (base
/// 0x180). Writing a `1` disables (masks) the corresponding interrupt.
const GICD_ICENABLER: usize = 0x180;
/// `GICD_IPRIORITYR<n>` — priority, one byte per interrupt (base 0x400).
const GICD_IPRIORITYR: usize = 0x400;
/// `GICD_ITARGETSR<n>` — interrupt processor targets, one byte per
/// interrupt (base 0x800). The byte is a CPU-interface bitmask: bit `c`
/// routes the interrupt to CPU `c`. The first 32 bytes (SGIs/PPIs) are
/// read-only and banked per CPU; only the SPI bytes (INTID `>= 32`) are
/// writable, which is why [`Gicv2::route_spi`] is the SPI-only routing
/// primitive (GICv2 spec §4.3.12).
const GICD_ITARGETSR: usize = 0x800;
/// `GICD_SGIR` — software-generated interrupt control (offset 0xF00).
const GICD_SGIR: usize = 0xF00;

/// `GICC_CTLR` — CPU-interface control (offset 0x000). Bit 0 enables
/// signalling of interrupts to the CPU.
const GICC_CTLR: usize = 0x000;
/// `GICC_PMR` — interrupt priority mask (offset 0x004). Only interrupts
/// of higher priority (numerically lower) than this are signalled.
const GICC_PMR: usize = 0x004;
/// `GICC_IAR` — interrupt acknowledge (offset 0x00C). A read returns the
/// INTID of the highest-priority pending interrupt and activates it.
const GICC_IAR: usize = 0x00C;
/// `GICC_EOIR` — end of interrupt (offset 0x010). Writing the INTID read
/// from `GICC_IAR` deactivates it.
const GICC_EOIR: usize = 0x010;

/// Highest addressable GICv2 INTID. INTIDs `1020..=1023` are reserved by
/// the spec (1023 is [`SPURIOUS_INTID`]); a controller rejects anything
/// above this as out of range (`AGENTS.md` §5.4.5 — fail closed).
pub const MAX_INTID: u32 = 1019;

/// Mask isolating the INTID from a `GICC_IAR` read (bits `[9:0]`; the
/// upper bits carry the source CPU for SGIs).
pub const IAR_INTID_MASK: u32 = 0x3FF;

/// Spurious-interrupt INTID the CPU interface returns from `GICC_IAR`
/// when no interrupt is pending (GICv2 spec §3.2.4). The handler must
/// not `EOI` it.
pub const SPURIOUS_INTID: u32 = 1023;

/// Word written to `GICD_SGIR` to raise SGI `intid` on the CPUs named in
/// `target_list` (one bit per CPU, bits `[23:16]`), with target-list
/// filter `0b00` ("forward to the listed CPUs").
#[must_use]
pub const fn sgir_value(intid: u32, target_list: u8) -> u32 {
    ((target_list as u32) << 16) | (intid & 0xF)
}

/// Byte offset of the `GICD_ISENABLER` word covering interrupt `intid`.
#[must_use]
pub const fn isenabler_offset(intid: u32) -> usize {
    GICD_ISENABLER + ((intid / 32) as usize) * 4
}

/// Bit position within the `GICD_ISENABLER` word for interrupt `intid`.
#[must_use]
pub const fn isenabler_bit(intid: u32) -> u32 {
    1 << (intid % 32)
}

/// Lowest GICv2 INTID that is a shared peripheral interrupt (SPI). INTIDs
/// `0..32` are SGIs/PPIs whose `GICD_ITARGETSR` bytes are read-only and
/// banked per CPU; only `>= MIN_SPI_INTID` may be routed with
/// [`Gicv2::route_spi`] (GICv2 spec §2.2.1).
pub const MIN_SPI_INTID: u32 = 32;

/// Byte offset of the `GICD_ITARGETSR` register for interrupt `intid`
/// (one byte per INTID).
#[must_use]
pub const fn itargetsr_offset(intid: u32) -> usize {
    GICD_ITARGETSR + intid as usize
}

/// Byte offset of the `GICD_ICENABLER` word covering interrupt `intid`.
/// Parallel layout to [`isenabler_offset`]; the bit position is shared
/// ([`isenabler_bit`]).
#[must_use]
pub const fn icenabler_offset(intid: u32) -> usize {
    GICD_ICENABLER + ((intid / 32) as usize) * 4
}

/// Volatile access to a GICv2 distributor + CPU-interface register pair.
///
/// The production implementation is `VolatileGicMmio` (freestanding
/// only); host tests substitute an in-memory mock. Modelled on riscv64's
/// `PlicMmio` seam so the whole controller control-flow is host-testable
/// (`AGENTS.md` §2.2 — one MMIO path, no duplicate register logic).
pub trait GicMmio {
    /// Read the distributor register at byte offset `off`.
    fn gicd_read(&self, off: usize) -> u32;
    /// Write the distributor 32-bit register at byte offset `off`.
    fn gicd_write(&self, off: usize, val: u32);
    /// Write the distributor byte register at byte offset `off` (the
    /// byte-addressable priority registers).
    fn gicd_write_byte(&self, off: usize, val: u8);
    /// Read the CPU-interface register at byte offset `off`.
    fn gicc_read(&self, off: usize) -> u32;
    /// Write the CPU-interface register at byte offset `off`.
    fn gicc_write(&self, off: usize, val: u32);
}

/// Low-level GICv2 register driver over a [`GicMmio`] seam.
///
/// Holds no policy: it exposes the raw enable/disable/priority/ack/EOI
/// operations and leaves range validation and the mask-before-wake fence
/// to [`GicController`].
pub struct Gicv2<M: GicMmio> {
    mmio: M,
}

impl<M: GicMmio> Gicv2<M> {
    /// Bind a driver to `mmio`.
    pub const fn new(mmio: M) -> Self {
        Self { mmio }
    }

    /// Enable the distributor and this CPU's interface and open the
    /// priority mask so every priority is signalled.
    pub fn init(&self) {
        self.mmio.gicc_write(GICC_PMR, 0xFF);
        self.mmio.gicc_write(GICC_CTLR, 1);
        self.mmio.gicd_write(GICD_CTLR, 1);
    }

    /// Give `intid` a mid-range priority and set its enable bit.
    pub fn enable_intid(&self, intid: u32) {
        self.mmio
            .gicd_write_byte(GICD_IPRIORITYR + intid as usize, 0x80);
        self.mmio
            .gicd_write(isenabler_offset(intid), isenabler_bit(intid));
    }

    /// Clear `intid`'s enable bit, masking it at the distributor.
    pub fn disable_intid(&self, intid: u32) {
        self.mmio
            .gicd_write(icenabler_offset(intid), isenabler_bit(intid));
    }

    /// Route shared-peripheral interrupt `intid` to the CPU interfaces
    /// named in `cpu_targets` (a bitmask: bit `c` selects CPU `c`).
    ///
    /// SPIs reset to *no* target on the GICv2, so a device's SPI is
    /// never delivered until its `GICD_ITARGETSR` byte names a CPU;
    /// this is the SPI analogue of the x86_64 IO-APIC redirection
    /// entry's destination field. INTIDs below [`MIN_SPI_INTID`] are
    /// SGIs/PPIs whose target bytes are read-only and banked per CPU, so
    /// the routing write is skipped for them (a no-op rather than a
    /// silently-ignored read-only store — `AGENTS.md` §5.4.5).
    pub fn route_spi(&self, intid: u32, cpu_targets: u8) {
        if intid >= MIN_SPI_INTID {
            self.mmio
                .gicd_write_byte(itargetsr_offset(intid), cpu_targets);
        }
    }

    /// Read `GICC_IAR` to acknowledge the highest-priority pending
    /// interrupt, returning its INTID (which may be [`SPURIOUS_INTID`]).
    #[must_use]
    pub fn acknowledge(&self) -> u32 {
        self.mmio.gicc_read(GICC_IAR) & IAR_INTID_MASK
    }

    /// Write `GICC_EOIR` to deactivate `intid`.
    pub fn end_of_interrupt(&self, intid: u32) {
        self.mmio.gicc_write(GICC_EOIR, intid);
    }

    /// Raise SGI INTID 0 on the CPUs named in `target`'s target-list bit.
    pub fn send_sgi(&self, target: CpuId) {
        let bit = u8::try_from(target)
            .ok()
            .and_then(|c| 1u8.checked_shl(u32::from(c)));
        if let Some(target_list) = bit {
            self.mmio.gicd_write(GICD_SGIR, sgir_value(0, target_list));
        }
    }
}

/// GICv2 controller: the policy layer over [`Gicv2`].
///
/// Validates every INTID against `max_intid` and fails closed
/// (`AGENTS.md` §5.4.5) before touching a register. Implements the
/// §17.2 Arch HAL [`rustos_arch_api::IrqController`] (line masking) and
/// [`rustos_arch_api::InterruptEntry`] (the claim/complete handshake).
pub struct GicController<M: GicMmio> {
    gic: Gicv2<M>,
    max_intid: u32,
}

impl<M: GicMmio> GicController<M> {
    /// Build a controller over `gic` whose highest valid INTID is
    /// `max_intid` (inclusive, clamped to [`MAX_INTID`]).
    #[must_use]
    pub const fn new(gic: Gicv2<M>, max_intid: u32) -> Self {
        let max_intid = if max_intid > MAX_INTID {
            MAX_INTID
        } else {
            max_intid
        };
        Self { gic, max_intid }
    }

    /// Inclusive upper bound on accepted INTIDs.
    #[must_use]
    pub const fn max_intid(&self) -> u32 {
        self.max_intid
    }

    const fn in_range(&self, intid: u32) -> bool {
        intid <= self.max_intid
    }
}

impl<M: GicMmio + Send + Sync> rustos_arch_api::IrqController for GicController<M> {
    /// Mask `line` by clearing its distributor enable bit, then emit a
    /// `SeqCst` fence so the masked state is globally visible before a
    /// waiter observes `ready = true` (`docs/src/security/irq.md`).
    fn mask(&self, line: u32) -> Result<(), rustos_arch_api::IrqControlError> {
        if !self.in_range(line) {
            return Err(rustos_arch_api::IrqControlError::OutOfRange);
        }
        self.gic.disable_intid(line);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Unmask `line` by setting its distributor enable bit (priority is
    /// left at the mid value [`Gicv2::enable_intid`] installs).
    fn unmask(&self, line: u32) -> Result<(), rustos_arch_api::IrqControlError> {
        if !self.in_range(line) {
            return Err(rustos_arch_api::IrqControlError::OutOfRange);
        }
        self.gic.enable_intid(line);
        Ok(())
    }
}

impl<M: GicMmio + Send + Sync> rustos_arch_api::InterruptEntry for GicController<M> {
    /// Acknowledge the active interrupt, mapping the GICv2
    /// [`SPURIOUS_INTID`] ("nothing pending") to [`None`]
    /// (`AGENTS.md` §17.2).
    fn claim(&self) -> Option<u32> {
        match self.gic.acknowledge() {
            SPURIOUS_INTID => None,
            intid => Some(intid),
        }
    }

    /// End-of-interrupt for `line`.
    fn complete(&self, line: u32) {
        self.gic.end_of_interrupt(line);
    }
}

/// Bare-metal [`GicMmio`] over the fixed `virt`-board GICv2 windows.
///
/// Compiled only for the freestanding aarch64 target; host builds use
/// the in-memory mock in the test module.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub struct VolatileGicMmio;

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
impl GicMmio for VolatileGicMmio {
    fn gicd_read(&self, off: usize) -> u32 {
        // SAFETY: `off` addresses a distributor register within the
        // fixed `virt`-board GICv2 MMIO window.
        unsafe { core::ptr::read_volatile((GICD_BASE + off) as *const u32) }
    }
    fn gicd_write(&self, off: usize, val: u32) {
        // SAFETY: as `gicd_read`, but a 32-bit store.
        unsafe { core::ptr::write_volatile((GICD_BASE + off) as *mut u32, val) }
    }
    fn gicd_write_byte(&self, off: usize, val: u8) {
        // SAFETY: the byte-addressable priority register for the INTID
        // inside the distributor window; QEMU honours the byte store.
        unsafe { core::ptr::write_volatile((GICD_BASE + off) as *mut u8, val) }
    }
    fn gicc_read(&self, off: usize) -> u32 {
        // SAFETY: `off` addresses a CPU-interface register within the
        // fixed `virt`-board GICv2 MMIO window.
        unsafe { core::ptr::read_volatile((GICC_BASE + off) as *const u32) }
    }
    fn gicc_write(&self, off: usize, val: u32) {
        // SAFETY: as `gicc_read`, but a 32-bit store.
        unsafe { core::ptr::write_volatile((GICC_BASE + off) as *mut u32, val) }
    }
}

/// Construct the production driver over the fixed `virt`-board windows.
///
/// # Safety
///
/// The GICv2 distributor + CPU interface must be mapped at the fixed
/// `virt`-board bases ([`GICD_BASE`] / [`GICC_BASE`]), identity-mapped
/// and exclusively owned by the kernel.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
const unsafe fn volatile_gic() -> Gicv2<VolatileGicMmio> {
    Gicv2::new(VolatileGicMmio)
}

/// Enable the distributor and the calling CPU's interface, and open the
/// priority mask so every priority is signalled.
///
/// # Safety
///
/// Must be called once per CPU during bring-up, before any interrupt
/// source is enabled. It writes the fixed GICv2 MMIO windows on the
/// `virt` board; calling it elsewhere is a kernel bug.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn init() {
    // SAFETY: bring-up context — the fixed `virt`-board windows are
    // mapped and owned by the kernel.
    unsafe { volatile_gic() }.init();
}

/// Enable private-peripheral / shared interrupt `intid` and give it a
/// mid-range priority.
///
/// # Safety
///
/// The distributor must already be enabled ([`init`]). Enabling an
/// INTID lets it reach the CPU once its source is armed.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn enable_ppi(intid: u32) {
    // SAFETY: as `init` — the fixed windows are mapped and owned.
    unsafe { volatile_gic() }.enable_intid(intid);
}

/// Route shared-peripheral interrupt `intid` to the CPU interfaces named
/// in `cpu_targets` (bit `c` selects CPU `c`).
///
/// SPIs reset to no target on the GICv2, so this must be called for a
/// device SPI before it can be delivered (see [`Gicv2::route_spi`]).
///
/// # Safety
///
/// The distributor must already be enabled ([`init`]); the fixed
/// `virt`-board windows are mapped and owned by the kernel.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn route_spi(intid: u32, cpu_targets: u8) {
    // SAFETY: as `init` — the fixed windows are mapped and owned.
    unsafe { volatile_gic() }.route_spi(intid, cpu_targets);
}

/// Read `GICC_IAR` to acknowledge and activate the highest-priority
/// pending interrupt, returning its INTID (which may be
/// [`SPURIOUS_INTID`]).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn acknowledge() -> u32 {
    // SAFETY: the fixed windows are mapped and owned by the kernel.
    unsafe { volatile_gic() }.acknowledge()
}

/// Write `GICC_EOIR` to deactivate the interrupt `intid` previously
/// returned by [`acknowledge`].
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn end_of_interrupt(intid: u32) {
    // SAFETY: the fixed windows are mapped and owned by the kernel.
    unsafe { volatile_gic() }.end_of_interrupt(intid);
}

/// Raise software-generated interrupt INTID 0 on `target`.
///
/// Used by [`crate::kernel_arch::Aarch64Arch`]'s `send_ipi`. A
/// single-CPU image targets itself, which the GIC delivers as a normal
/// SGI.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn send_sgi(target: CpuId) {
    // SAFETY: the fixed windows are mapped and owned by the kernel.
    unsafe { volatile_gic() }.send_sgi(target);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgir_packs_target_list_and_intid() {
        // INTID 0 to CPU 0 → target list bit 0.
        assert_eq!(sgir_value(0, 0b0000_0001), 1 << 16);
        // INTID 0 to CPU 2 → target list bit 2.
        assert_eq!(sgir_value(0, 0b0000_0100), 0b0000_0100 << 16);
        // INTID field is the low 4 bits only.
        assert_eq!(sgir_value(0x1F, 1) & 0xF, 0xF);
    }

    #[test]
    fn isenabler_offset_selects_the_right_word() {
        // INTIDs 0..32 live in the first word (offset 0x100).
        assert_eq!(isenabler_offset(0), 0x100);
        assert_eq!(isenabler_offset(30), 0x100);
        // INTID 32 spills into the second word.
        assert_eq!(isenabler_offset(32), 0x104);
    }

    #[test]
    fn isenabler_bit_indexes_within_the_word() {
        assert_eq!(isenabler_bit(0), 1);
        assert_eq!(isenabler_bit(30), 1 << 30);
        assert_eq!(isenabler_bit(32), 1);
    }

    #[test]
    fn iar_mask_and_spurious_match_gicv2_spec() {
        assert_eq!(IAR_INTID_MASK, 0x3FF);
        assert_eq!(SPURIOUS_INTID, 1023);
    }

    #[test]
    fn icenabler_offset_parallels_isenabler() {
        assert_eq!(icenabler_offset(0), 0x180);
        assert_eq!(icenabler_offset(30), 0x180);
        assert_eq!(icenabler_offset(32), 0x184);
    }

    #[test]
    fn itargetsr_offset_is_one_byte_per_intid() {
        // SPI 2 on the `virt` board (the PL031 RTC) is INTID 34.
        assert_eq!(itargetsr_offset(34), 0x800 + 34);
        assert_eq!(itargetsr_offset(MIN_SPI_INTID), 0x800 + 32);
    }

    #[test]
    fn route_spi_writes_the_target_byte_for_an_spi() {
        let gic = Gicv2::new(MockGicMmio::new());
        // Route INTID 34 to CPU 0 (target-list bit 0).
        gic.route_spi(34, 0b0000_0001);
        assert_eq!(gic.mmio.gicd_read(itargetsr_offset(34)), 0b0000_0001);
    }

    #[test]
    fn route_spi_skips_sgis_and_ppis() {
        let gic = Gicv2::new(MockGicMmio::new());
        // INTID 30 is the timer PPI: its target byte is read-only and
        // banked, so `route_spi` must not write it.
        gic.route_spi(30, 0b0000_0001);
        assert_eq!(gic.mmio.gicd_read(itargetsr_offset(30)), 0);
    }

    /// In-memory GICv2 register file: distributor and CPU-interface
    /// windows are independent, so the mock keeps a map per window and
    /// serves the last value written to a register on a subsequent read.
    struct MockGicMmio {
        gicd: std::sync::Mutex<std::collections::HashMap<usize, u32>>,
        gicc: std::sync::Mutex<std::collections::HashMap<usize, u32>>,
    }

    impl MockGicMmio {
        fn new() -> Self {
            Self {
                gicd: std::sync::Mutex::new(std::collections::HashMap::new()),
                gicc: std::sync::Mutex::new(std::collections::HashMap::new()),
            }
        }
    }

    impl GicMmio for MockGicMmio {
        fn gicd_read(&self, off: usize) -> u32 {
            *self.gicd.lock().unwrap().get(&off).unwrap_or(&0)
        }
        fn gicd_write(&self, off: usize, val: u32) {
            self.gicd.lock().unwrap().insert(off, val);
        }
        fn gicd_write_byte(&self, off: usize, val: u8) {
            self.gicd.lock().unwrap().insert(off, u32::from(val));
        }
        fn gicc_read(&self, off: usize) -> u32 {
            *self.gicc.lock().unwrap().get(&off).unwrap_or(&0)
        }
        fn gicc_write(&self, off: usize, val: u32) {
            self.gicc.lock().unwrap().insert(off, val);
        }
    }

    #[test]
    fn enable_intid_sets_priority_and_enable_bit() {
        let gic = Gicv2::new(MockGicMmio::new());
        gic.enable_intid(42);
        assert_eq!(gic.mmio.gicd_read(GICD_IPRIORITYR + 42), 0x80);
        assert_eq!(
            gic.mmio.gicd_read(isenabler_offset(42)) & isenabler_bit(42),
            isenabler_bit(42)
        );
    }

    #[test]
    fn disable_intid_sets_clear_enable_bit() {
        let gic = Gicv2::new(MockGicMmio::new());
        gic.disable_intid(42);
        assert_eq!(
            gic.mmio.gicd_read(icenabler_offset(42)) & isenabler_bit(42),
            isenabler_bit(42)
        );
    }

    #[test]
    fn acknowledge_masks_off_the_source_cpu_bits() {
        let gic = Gicv2::new(MockGicMmio::new());
        // A real IAR carries the source CPU in the upper bits for an SGI;
        // `acknowledge` returns only the INTID field.
        gic.mmio.gicc_write(GICC_IAR, (0b101 << 10) | 0x2A);
        assert_eq!(gic.acknowledge(), 0x2A);
    }

    #[test]
    fn controller_clamps_max_intid_to_the_spec_ceiling() {
        let c = GicController::new(Gicv2::new(MockGicMmio::new()), u32::MAX);
        assert_eq!(c.max_intid(), MAX_INTID);
    }

    /// §17.2 / W3: the GIC controller passes the shared Arch HAL
    /// interrupt-controller + interrupt-entry conformance verticals over
    /// its real handle (`plans/WIRING.md` Stage W3). INTID 42 is an
    /// addressable SPI; 2000 is above [`MAX_INTID`]. The mock's `GICC_IAR`
    /// is seeded with [`SPURIOUS_INTID`] so the [`InterruptEntry`] drain
    /// terminates ("nothing pending").
    #[test]
    fn gic_controller_passes_arch_hal_irq_conformance() {
        use rustos_arch_api::{InterruptEntry, IrqController};

        let c = GicController::new(Gicv2::new(MockGicMmio::new()), 1019);
        c.gic.mmio.gicc_write(GICC_IAR, SPURIOUS_INTID);

        rustos_arch_api::irq::conformance::run_controller(&c, 42, 2000);
        rustos_arch_api::irq::conformance::run_entry(&c);

        // Object-safe behind `&dyn`, the way the kernel reaches it.
        let dyn_ctrl: &dyn IrqController = &c;
        assert_eq!(dyn_ctrl.mask(42), Ok(()));
        let dyn_entry: &dyn InterruptEntry = &c;
        assert_eq!(dyn_entry.claim(), None);
    }
}
