//! Host unit test for [`PlicIrqController`] (`AGENTS.md` §7 — tests in
//! their own file).
//!
//! Pins the mask-before-wake contract end to end: driving
//! [`rustos_kernel_irq::IrqTable::fire`] through the bridge must mask
//! the PLIC source (priority → 0) before `fire` returns `Marked`, i.e.
//! before any waiter can observe `ready = true`. The arch port owns no
//! `kernel/irq` dependency, so this contract is pinned here, where the
//! `IrqController` bridge lives (`AGENTS.md` §17.2).

use super::*;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use rustos_arch_riscv64::plic::{regs, s_mode_context, Plic, PlicController, PlicMmio};
use rustos_kernel_irq::{FireOutcome, IrqController, IrqTable};
use rustos_kernel_sec::captable::TaskId;

/// In-memory PLIC register file. Serves the last value written to a
/// register on a subsequent read and records every write in order
/// through a shared log so the test can assert the write sequence
/// (`PlicController`'s fields are private to the arch crate, so the log
/// is the only seam into the write ordering).
struct MockPlicMmio {
    cells: Mutex<HashMap<usize, u32>>,
    writes: Arc<Mutex<Vec<(usize, u32)>>>,
}

impl MockPlicMmio {
    fn new(writes: Arc<Mutex<Vec<(usize, u32)>>>) -> Self {
        Self {
            cells: Mutex::new(HashMap::new()),
            writes,
        }
    }
}

impl PlicMmio for MockPlicMmio {
    fn read32(&self, offset: usize) -> u32 {
        *self.cells.lock().unwrap().get(&offset).unwrap_or(&0)
    }

    fn write32(&self, offset: usize, value: u32) {
        self.cells.lock().unwrap().insert(offset, value);
        self.writes.lock().unwrap().push((offset, value));
    }
}

#[test]
fn mask_before_wake_through_irq_table() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let mock = MockPlicMmio::new(Arc::clone(&writes));
    let controller =
        PlicIrqController::new(PlicController::new(Plic::new(mock, s_mode_context(0)), 31));

    // Arm the source; it now delivers (non-zero priority).
    controller.arm(8).expect("arm");
    assert_eq!(controller.source_priority(8), 1);

    let table = IrqTable::new(31);
    let _bind = table.bind(8, TaskId(1)).expect("bind");
    let outcome = table
        .fire(8, &controller as &dyn IrqController)
        .expect("fire");
    assert_eq!(outcome, FireOutcome::Marked);

    // The mask write (priority → 0) must have landed by the time `fire`
    // returned, and it must be the last register write in the sequence.
    assert_eq!(controller.source_priority(8), 0);
    let log = writes.lock().unwrap();
    let last = log.last().copied().expect("at least one write");
    assert_eq!(last, (regs::source_priority(8), 0));
}

#[test]
fn mask_rejects_out_of_range_source() {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let mock = MockPlicMmio::new(writes);
    let controller =
        PlicIrqController::new(PlicController::new(Plic::new(mock, s_mode_context(0)), 15));
    // Source 0 is reserved and 16 is above `max_source`; both map onto
    // `MaskError::OutOfRange` through the bridge.
    assert!(IrqController::mask(&controller, 0).is_err());
    assert!(IrqController::mask(&controller, 16).is_err());
}
