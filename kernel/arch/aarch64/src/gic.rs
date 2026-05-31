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

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
use rustos_arch_api::CpuId;

/// MMIO base of the GICv2 distributor on the `virt` board.
pub const GICD_BASE: usize = 0x0800_0000;

/// MMIO base of the GICv2 CPU interface on the `virt` board.
pub const GICC_BASE: usize = 0x0801_0000;

/// `GICD_CTLR` — distributor control (offset 0x000). Bit 0 enables
/// forwarding of pending interrupts to the CPU interfaces.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
const GICD_CTLR: usize = 0x000;
/// `GICD_ISENABLER<n>` — set-enable, one bit per interrupt (base 0x100).
const GICD_ISENABLER: usize = 0x100;
/// `GICD_IPRIORITYR<n>` — priority, one byte per interrupt (base 0x400).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
const GICD_IPRIORITYR: usize = 0x400;
/// `GICD_SGIR` — software-generated interrupt control (offset 0xF00).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
const GICD_SGIR: usize = 0xF00;

/// `GICC_CTLR` — CPU-interface control (offset 0x000). Bit 0 enables
/// signalling of interrupts to the CPU.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
const GICC_CTLR: usize = 0x000;
/// `GICC_PMR` — interrupt priority mask (offset 0x004). Only interrupts
/// of higher priority (numerically lower) than this are signalled.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
const GICC_PMR: usize = 0x004;
/// `GICC_IAR` — interrupt acknowledge (offset 0x00C). A read returns the
/// INTID of the highest-priority pending interrupt and activates it.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
const GICC_IAR: usize = 0x00C;
/// `GICC_EOIR` — end of interrupt (offset 0x010). Writing the INTID read
/// from `GICC_IAR` deactivates it.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
const GICC_EOIR: usize = 0x010;

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

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod mmio {
    use super::{GICC_BASE, GICD_BASE};

    pub(super) fn gicd_write(off: usize, val: u32) {
        // SAFETY: `off` addresses a distributor register within the
        // fixed `virt`-board GICv2 MMIO window; a 32-bit store.
        unsafe { core::ptr::write_volatile((GICD_BASE + off) as *mut u32, val) }
    }

    pub(super) fn gicc_read(off: usize) -> u32 {
        // SAFETY: `off` addresses a CPU-interface register within the
        // fixed `virt`-board GICv2 MMIO window.
        unsafe { core::ptr::read_volatile((GICC_BASE + off) as *const u32) }
    }

    pub(super) fn gicc_write(off: usize, val: u32) {
        // SAFETY: as `gicc_read`, but a 32-bit store.
        unsafe { core::ptr::write_volatile((GICC_BASE + off) as *mut u32, val) }
    }
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
    // Enable the CPU interface and open the priority mask to the lowest
    // priority so no interrupt is masked by priority.
    mmio::gicc_write(GICC_PMR, 0xFF);
    mmio::gicc_write(GICC_CTLR, 1);
    // Enable the distributor (group 0 forwarding).
    mmio::gicd_write(GICD_CTLR, 1);
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
    // Mid priority (0x80) so the open PMR (0xFF) signals it.
    let prio_off = GICD_IPRIORITYR + intid as usize;
    // Priority registers are byte-accessible; QEMU's model honours a
    // byte store here.
    // SAFETY: `prio_off` is the priority byte for `intid` in the
    // distributor window.
    unsafe {
        core::ptr::write_volatile((GICD_BASE + prio_off) as *mut u8, 0x80);
    }
    mmio::gicd_write(isenabler_offset(intid), isenabler_bit(intid));
}

/// Read `GICC_IAR` to acknowledge and activate the highest-priority
/// pending interrupt, returning its INTID (which may be
/// [`SPURIOUS_INTID`]).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn acknowledge() -> u32 {
    mmio::gicc_read(GICC_IAR) & IAR_INTID_MASK
}

/// Write `GICC_EOIR` to deactivate the interrupt `intid` previously
/// returned by [`acknowledge`].
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn end_of_interrupt(intid: u32) {
    mmio::gicc_write(GICC_EOIR, intid);
}

/// Raise software-generated interrupt INTID 0 on `target`.
///
/// Used by [`crate::kernel_arch::Aarch64Arch`]'s `send_ipi`. A
/// single-CPU image targets itself, which the GIC delivers as a normal
/// SGI.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn send_sgi(target: CpuId) {
    // One bit per CPU in the target list; clamp to the 8-bit field.
    let bit = u8::try_from(target)
        .ok()
        .and_then(|c| 1u8.checked_shl(u32::from(c)));
    if let Some(target_list) = bit {
        mmio::gicd_write(GICD_SGIR, sgir_value(0, target_list));
    }
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
}
