//! Host unit tests for the PLIC register driver (tests live in their own file beside the code they cover).
//!
//! These exercise the register arithmetic and the inherent
//! arm/mask/unmask/claim surface against an in-memory mock. The
//! mask-before-wake contract through `kernel/irq`'s `IrqTable` is
//! exercised downstream, where the `IrqController` bridge lives
//! (the arch port owns no `kernel/irq` dependency).

use super::*;
use std::collections::HashMap;
use std::sync::Mutex;

/// In-memory PLIC register file: serves the last value written to a
/// register on a subsequent read.
struct MockPlicMmio {
    cells: Mutex<HashMap<usize, u32>>,
}

impl MockPlicMmio {
    fn new() -> Self {
        Self {
            cells: Mutex::new(HashMap::new()),
        }
    }
}

impl PlicMmio for MockPlicMmio {
    fn read32(&self, offset: usize) -> u32 {
        *self.cells.lock().unwrap().get(&offset).unwrap_or(&0)
    }

    fn write32(&self, offset: usize, value: u32) {
        self.cells.lock().unwrap().insert(offset, value);
    }
}

fn controller(max_source: u32) -> PlicController<MockPlicMmio> {
    PlicController::new(
        Plic::new(MockPlicMmio::new(), s_mode_context(0)),
        max_source,
    )
}

#[test]
fn s_mode_context_interleaves_per_hart() {
    assert_eq!(s_mode_context(0), 1);
    assert_eq!(s_mode_context(1), 3);
    assert_eq!(s_mode_context(3), 7);
}

#[test]
fn register_offsets_match_sifive_layout() {
    // Source 0 priority at base; source 1 four bytes up.
    assert_eq!(regs::source_priority(0), 0x0000);
    assert_eq!(regs::source_priority(1), 0x0004);
    // Context 1 enable bitmap: base + 1*0x80; source 32 is the second
    // word.
    assert_eq!(regs::enable_word(1, 1), 0x2080);
    assert_eq!(regs::enable_word(1, 32), 0x2084);
    assert_eq!(regs::enable_bit(1), 0x2);
    assert_eq!(regs::enable_bit(32), 0x1);
    // Context 1 threshold/claim block at 0x20_0000 + 0x1000.
    assert_eq!(regs::threshold(1), 0x0020_1000);
    assert_eq!(regs::claim(1), 0x0020_1004);
}

#[test]
fn arm_enables_source_sets_priority_and_threshold() {
    let c = controller(31);
    c.arm(8).expect("arm in range");
    // Threshold dropped to zero, source enabled, priority delivering.
    assert_eq!(c.source_priority(8), ACTIVE_PRIORITY);
    let plic = &c.plic;
    assert_eq!(plic.mmio.read32(regs::threshold(1)), 0);
    let enable_word = plic.mmio.read32(regs::enable_word(1, 8));
    assert_eq!(enable_word & regs::enable_bit(8), regs::enable_bit(8));
}

#[test]
fn arm_rejects_out_of_range_source() {
    let c = controller(31);
    assert_eq!(c.arm(0), Err(PlicError::SourceOutOfRange));
    assert_eq!(c.arm(32), Err(PlicError::SourceOutOfRange));
    // Boundary: max_source itself is accepted.
    assert_eq!(c.arm(31), Ok(()));
}

#[test]
fn mask_drops_priority_to_zero() {
    let c = controller(31);
    c.arm(8).expect("arm");
    c.mask(8).expect("mask");
    assert_eq!(c.source_priority(8), MASKED_PRIORITY);
}

#[test]
fn unmask_restores_delivering_priority() {
    let c = controller(31);
    c.arm(8).expect("arm");
    c.mask(8).expect("mask");
    c.unmask(8).expect("unmask");
    assert_eq!(c.source_priority(8), ACTIVE_PRIORITY);
}

#[test]
fn mask_rejects_source_zero_and_out_of_range() {
    let c = controller(15);
    assert_eq!(c.mask(0), Err(PlicError::SourceOutOfRange));
    assert_eq!(c.mask(16), Err(PlicError::SourceOutOfRange));
}

#[test]
fn enable_then_disable_source_toggles_the_bitmap_bit() {
    let plic = Plic::new(MockPlicMmio::new(), s_mode_context(0));
    plic.enable_source(8);
    let off = regs::enable_word(1, 8);
    assert_eq!(
        plic.mmio.read32(off) & regs::enable_bit(8),
        regs::enable_bit(8)
    );
    plic.disable_source(8);
    assert_eq!(plic.mmio.read32(off) & regs::enable_bit(8), 0);
    assert_eq!(plic.context(), 1);
}

#[test]
fn claim_and_complete_round_trip_the_claim_register() {
    let c = controller(31);
    // A pending claim is whatever the PLIC reports; seed the mock.
    c.plic.mmio.write32(regs::claim(1), 8);
    assert_eq!(c.claim(), 8);
    c.complete(8);
    // Complete writes the source back to the same register.
    assert_eq!(c.plic.mmio.read32(regs::claim(1)), 8);
}

/// / W3: the PLIC controller passes the shared Arch HAL
/// interrupt-controller + interrupt-entry conformance verticals over its
/// real handle (`plans/WIRING.md` Stage W3). Source `8` is addressable on
/// a `max_source = 31` controller; `32` is out of range. The empty mock
/// reports no pending interrupt, exercising the [`InterruptEntry`] drain's
/// terminating path.
#[test]
fn plic_controller_passes_arch_hal_irq_conformance() {
    let c = controller(31);
    tairix_arch_api::irq::conformance::run_controller(&c, 8, 32);
    tairix_arch_api::irq::conformance::run_entry(&c);
}
