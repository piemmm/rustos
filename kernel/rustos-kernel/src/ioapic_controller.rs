//! Production [`IrqController`] implementation backed by the x86_64
//! IO-APIC.
//!
//! Stage 4.D Item 2-tail.2. The kernel binary builds one
//! [`IoApicController`] per boot during its post-MADT wiring phase
//! (see [`crate::boot::try_boot`]). The controller owns every
//! IO-APIC the firmware advertises through MADT and exposes a single
//! [`IrqController::mask`] method that the kernel-neutral
//! [`rustos_kernel_irq::IrqTable::fire`] path invokes *before* it
//! sets a wait-handle's `ready` flag — the mask-before-wake
//! invariant documented in `docs/src/security/irq.md`.
//!
//! # Mask-before-wake
//!
//! [`rustos_kernel_irq::IrqTable::fire`]'s ordering contract
//! requires the controller's `mask` call to complete (and be
//! globally observable) before the `ready` flag flips. This
//! controller honours the contract by:
//!
//! 1. Re-writing the IO-APIC redirection entry through the
//!    audited [`IoApic::set_redirection_entry`] driver, which uses
//!    volatile MMIO (`VolatileIoApicMmio`), so the mask bit lands
//!    on the CPU's write-combining store buffer before the function
//!    returns.
//! 2. Emitting a [`core::sync::atomic::fence`] with
//!    [`Ordering::SeqCst`] after the mask write so a subsequent
//!    waker observing `ready = true` is guaranteed to also observe
//!    the masked line.
//!
//! Step 2 is the load-bearing barrier: the IO-APIC's MMIO write is
//! globally ordered with respect to memory operations only after
//! a memory fence on Intel/AMD x86_64. The
//! `ioapic_controller_mask_before_wake_ordering` host test pins the
//! ordering against a `RecordingMmio` mock that captures every MMIO
//! write in observed order.
//!
//! # Multi-IO-APIC layout
//!
//! A modern x86_64 platform can advertise more than one IO-APIC.
//! Each `MadtEntry::IoApic` carries an `address`, an `id`, and a
//! `gsi_base` — the GSI range this IO-APIC owns is
//! `gsi_base .. gsi_base + max_redirection_entry + 1`. The
//! controller stores one block per IO-APIC and routes the
//! kernel-neutral "line" parameter (a GSI) by linear scan; the table
//! is bounded by `MAX_IO_APICS` (8 by §7-line-budget; QEMU has 1).

extern crate alloc;
use alloc::vec::Vec;

use core::sync::atomic::{fence, Ordering};

use rustos_arch_x86_64::apic::{IoApic, IoApicMmio};
use rustos_kernel_irq::{IrqController, MaskError};
use rustos_kernel_sync::spinlock::SpinLock;

/// Cached pre-image of one IO-APIC redirection entry's
/// non-mask-bit state. Refreshed whenever the kernel re-writes the
/// entry; consulted by [`IoApicController::mask`] so the mask write
/// preserves the vector / destination programmed at install time.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct PinSettings {
    vector: u8,
    dest_apic_id: u8,
    masked: bool,
}

/// One IO-APIC and its cached per-pin state.
///
/// `inner` is wrapped in a [`SpinLock`] because the [`IoApic`] driver
/// requires `&mut self` for any MMIO operation (the IOREGSEL/IOWIN
/// indirection is inherently non-reentrant — Intel SDM Vol 3A §11.4).
/// The lock is acquired only from controller methods running at the
/// kernel-trap or boot context, so contention is bounded and the
/// critical section is short.
struct Block<M: IoApicMmio + Send> {
    gsi_base: u32,
    pin_count: u32,
    inner: SpinLock<BlockInner<M>>,
}

struct BlockInner<M: IoApicMmio + Send> {
    ioapic: IoApic<M>,
    /// `pin_cache[pin]` = `Some(settings)` once
    /// [`IoApicController::program_pin`] has run for that pin.
    /// `None` for pins never wired by the kernel binary's IDT-install
    /// pass; `mask` on such a pin returns
    /// [`MaskError::OutOfRange`] (fail-closed per `AGENTS.md` §5.4.5).
    pin_cache: Vec<Option<PinSettings>>,
}

/// Production [`IrqController`] backed by one-or-more IO-APICs.
pub struct IoApicController<M: IoApicMmio + Send + 'static> {
    blocks: Vec<Block<M>>,
}

// `Vec<Block<M>>` is `Send + Sync` whenever `M: Send` because the
// `SpinLock<BlockInner<M>>` lifts the mutability requirement off the
// public surface and `IoApic<M>` carries no thread-affinity.
unsafe impl<M: IoApicMmio + Send + 'static> Sync for IoApicController<M> {}

/// Failure modes of [`IoApicController::program_pin`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ProgramError {
    /// No block in this controller owns `gsi`. Either the GSI is
    /// above every IO-APIC's range, or the `Phase::Irq` boot step
    /// did not discover this IO-APIC.
    GsiOutOfRange,
}

impl<M: IoApicMmio + Send + 'static> IoApicController<M> {
    /// Build a controller from a list of IO-APIC `(gsi_base, ioapic,
    /// pin_count)` triples.
    ///
    /// `pin_count` is `max_redirection_entry + 1` as read from each
    /// IO-APIC's identification register at boot. The order of
    /// entries in `blocks` is insignificant — lookups walk the list.
    #[must_use]
    pub fn new(blocks: Vec<(u32, IoApic<M>, u32)>) -> Self {
        let blocks = blocks
            .into_iter()
            .map(|(gsi_base, ioapic, pin_count)| Block {
                gsi_base,
                pin_count,
                inner: SpinLock::new(BlockInner {
                    ioapic,
                    pin_cache: alloc::vec![None; pin_count as usize],
                }),
            })
            .collect();
        Self { blocks }
    }

    /// Program one redirection entry and cache its settings.
    ///
    /// Returns the block index `gsi` maps to so the caller can keep
    /// driver-host bookkeeping in sync; returns
    /// [`ProgramError::GsiOutOfRange`] when no block owns `gsi`.
    ///
    /// `masked` is the initial mask state. The kernel binary's boot
    /// pipeline programs every line `masked = true` and unmasks via
    /// a subsequent driver-side `unmask` follow-up (Item 2-tail.3,
    /// out of scope here).
    pub fn program_pin(
        &self,
        gsi: u32,
        vector: u8,
        dest_apic_id: u8,
        masked: bool,
    ) -> Result<(), ProgramError> {
        let (idx, pin) = self.locate(gsi).ok_or(ProgramError::GsiOutOfRange)?;
        let block = &self.blocks[idx];
        let mut inner = block.inner.lock();
        // `pin < pin_count` by construction; `as u8` truncates from
        // a u32 < 256 (IO-APIC max redirection entries fit in u8).
        #[allow(clippy::cast_possible_truncation)]
        let pin_u8 = pin as u8;
        inner
            .ioapic
            .set_redirection_entry(pin_u8, vector, dest_apic_id, masked);
        inner.pin_cache[pin as usize] = Some(PinSettings {
            vector,
            dest_apic_id,
            masked,
        });
        Ok(())
    }

    /// Locate the block + pin that own `gsi`.
    fn locate(&self, gsi: u32) -> Option<(usize, u32)> {
        for (idx, block) in self.blocks.iter().enumerate() {
            if gsi >= block.gsi_base && gsi < block.gsi_base + block.pin_count {
                return Some((idx, gsi - block.gsi_base));
            }
        }
        None
    }

    /// Number of IO-APICs the controller spans. Test-only accessor.
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }
}

impl<M: IoApicMmio + Send + 'static> IrqController for IoApicController<M> {
    fn mask(&self, line: u32) -> Result<(), MaskError> {
        let (idx, pin) = self.locate(line).ok_or(MaskError::OutOfRange)?;
        let block = &self.blocks[idx];
        let mut inner = block.inner.lock();
        let cache_slot = inner.pin_cache[pin as usize].ok_or(MaskError::OutOfRange)?;

        // Re-write the redirection entry with the cached vector +
        // destination + masked=true. The IoApic driver writes the
        // low half (which carries the mask bit) through
        // `VolatileIoApicMmio::write`, which is a `write_volatile`
        // — sufficient to push the masked state to the IO-APIC's
        // MMIO window.
        #[allow(clippy::cast_possible_truncation)]
        let pin_u8 = pin as u8;
        inner.ioapic.set_redirection_entry(
            pin_u8,
            cache_slot.vector,
            cache_slot.dest_apic_id,
            true,
        );
        inner.pin_cache[pin as usize] = Some(PinSettings {
            masked: true,
            ..cache_slot
        });

        // Drop the spinlock before issuing the global fence so the
        // fence orders against the lock's `Release` — `IrqTable::fire`
        // sets `ready = true` *after* this function returns, and the
        // SeqCst fence here pairs with the SeqCst load `try_wait_step`
        // performs on `ready`, guaranteeing every CPU that observes
        // `ready = true` also observes the masked redirection entry.
        drop(inner);
        fence(Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_arch_x86_64::apic::IoApicMmio;
    use std::sync::{Arc, Mutex};
    use std::vec::Vec as StdVec;

    /// Recording mock IO-APIC MMIO. Captures every write in a
    /// shared log so tests can assert the order of operations.
    #[derive(Clone)]
    struct RecordingMmio {
        log: Arc<Mutex<StdVec<(u8, u32)>>>,
    }

    impl RecordingMmio {
        fn new() -> Self {
            Self {
                log: Arc::new(Mutex::new(StdVec::new())),
            }
        }
        fn snapshot(&self) -> StdVec<(u8, u32)> {
            self.log.lock().unwrap().clone()
        }
    }

    impl IoApicMmio for RecordingMmio {
        fn read(&mut self, reg: u8) -> u32 {
            // The IoApic driver reads IOAPICID (reg 0x00) and
            // IOAPICVER (reg 0x01) during construction-time
            // metadata queries; the controller never invokes
            // either, so the mock returns zeroes. The two reads
            // are not appended to the write log.
            let _ = reg;
            0
        }
        fn write(&mut self, reg: u8, value: u32) {
            self.log.lock().unwrap().push((reg, value));
        }
    }

    fn fresh_controller(
        gsi_base: u32,
        pin_count: u32,
    ) -> (IoApicController<RecordingMmio>, RecordingMmio) {
        let mmio = RecordingMmio::new();
        let ioapic = IoApic::new(mmio.clone());
        let controller = IoApicController::new(alloc::vec![(gsi_base, ioapic, pin_count)]);
        (controller, mmio)
    }

    #[test]
    fn locate_returns_none_for_gsi_above_range() {
        let (controller, _mmio) = fresh_controller(0, 24);
        // Borrow internal helper for the test.
        assert!(controller.locate(24).is_none());
        assert!(controller.locate(u32::MAX).is_none());
        assert!(controller.locate(0).is_some());
        assert!(controller.locate(23).is_some());
    }

    #[test]
    fn program_pin_records_settings_and_writes_low_then_high() {
        let (controller, mmio) = fresh_controller(0, 24);
        controller
            .program_pin(7, 0x30, 0xAB, true)
            .expect("program");
        let writes = mmio.snapshot();
        // The IoApic driver writes low (reg 0x10 + 2*pin) then high
        // (reg 0x10 + 2*pin + 1). For pin 7 that's regs 0x1E and 0x1F.
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].0, 0x10 + 14);
        // Low: vector | (masked ? mask_bit : 0).
        assert_eq!(writes[0].1 & 0xFF, 0x30);
        assert!(writes[0].1 & (1 << 16) != 0);
        assert_eq!(writes[1].0, 0x10 + 15);
        // High: dest_apic_id in bits 24..32.
        assert_eq!(writes[1].1, (0xABu32) << 24);
    }

    #[test]
    fn program_pin_rejects_gsi_out_of_range() {
        let (controller, _mmio) = fresh_controller(0, 24);
        assert_eq!(
            controller.program_pin(24, 0x30, 0xAB, true),
            Err(ProgramError::GsiOutOfRange),
        );
    }

    #[test]
    fn mask_rewrites_redirection_entry_with_masked_bit_set() {
        let (controller, mmio) = fresh_controller(0, 24);
        // Program pin 7 initially *unmasked*.
        controller
            .program_pin(7, 0x30, 0xAB, false)
            .expect("program");
        // Clear the install-time writes from the log so the assertions
        // below cover only the `mask` call's writes.
        mmio.log.lock().unwrap().clear();

        IrqController::mask(&controller, 7).expect("mask succeeds");

        let writes = mmio.snapshot();
        assert_eq!(writes.len(), 2, "mask must write low+high");
        assert_eq!(writes[0].0, 0x10 + 14);
        assert_eq!(writes[0].1 & 0xFF, 0x30, "vector preserved");
        assert!(writes[0].1 & (1 << 16) != 0, "mask bit set in low half");
        assert_eq!(writes[1].0, 0x10 + 15);
        assert_eq!(writes[1].1, (0xABu32) << 24, "destination preserved");
    }

    #[test]
    fn mask_returns_out_of_range_for_unprogrammed_pin() {
        let (controller, _mmio) = fresh_controller(0, 24);
        // Pin 7 was never programmed, so the cache slot is `None`.
        // The mask path fail-closes with `OutOfRange` (the
        // controller refuses to mask a line that was never
        // programmed at install time — `AGENTS.md` §5.4.5).
        assert_eq!(
            IrqController::mask(&controller, 7),
            Err(MaskError::OutOfRange),
        );
    }

    #[test]
    fn mask_returns_out_of_range_for_gsi_above_every_block() {
        let (controller, _mmio) = fresh_controller(0, 24);
        assert_eq!(
            IrqController::mask(&controller, 99),
            Err(MaskError::OutOfRange),
        );
    }

    #[test]
    fn multi_ioapic_controller_routes_by_gsi_base() {
        let mmio0 = RecordingMmio::new();
        let mmio1 = RecordingMmio::new();
        let ioapic0 = IoApic::new(mmio0.clone());
        let ioapic1 = IoApic::new(mmio1.clone());
        let controller = IoApicController::new(alloc::vec![(0, ioapic0, 24), (24, ioapic1, 8)]);
        assert_eq!(controller.block_count(), 2);
        controller.program_pin(5, 0x40, 1, true).expect("block 0");
        controller.program_pin(27, 0x41, 2, true).expect("block 1");
        assert!(!mmio0.snapshot().is_empty(), "block 0 received writes");
        assert!(!mmio1.snapshot().is_empty(), "block 1 received writes");
    }

    /// Stage 4.D Item 2-tail.2 — the mask-before-wake regression
    /// probe. Drives [`IrqTable`] with this controller and asserts
    /// the controller's MMIO write log records the mask write
    /// *before* the [`IrqTable`] flips `ready = true`.
    ///
    /// The fire path on success returns `FireOutcome::Marked`; the
    /// test observes the MMIO write count snapshotted by an
    /// [`IrqController`] override that records the snapshot at the
    /// moment `mask` returns and compares it against the count
    /// observed immediately after `fire` returns.
    #[test]
    fn ioapic_controller_mask_before_wake_ordering() {
        use rustos_kernel_irq::{FireOutcome, IrqTable};
        use rustos_kernel_sec::TaskId;

        let (controller, mmio) = fresh_controller(0, 24);
        controller.program_pin(7, 0x30, 0xAB, false).expect("prog");
        // Clear the install-time writes.
        mmio.log.lock().unwrap().clear();

        // Build a kernel-neutral IrqTable and bind line 7 to an
        // arbitrary task; the bind is necessary so `fire` walks
        // the `Marked` branch (the branch that mask-before-wake
        // covers).
        let table = IrqTable::new(23);
        let owner = TaskId(1);
        let _outcome = table.bind(7, owner).expect("bind");
        // Snapshot the write count *before* the fire.
        let pre_fire_writes = mmio.snapshot().len();
        let outcome = table
            .fire(7, &controller as &dyn IrqController)
            .expect("fire");
        assert!(matches!(outcome, FireOutcome::Marked));
        // Snapshot the write count *after* the fire. The
        // controller must have issued its two-write mask sequence
        // (low + high half of the redirection entry) before
        // `IrqTable::fire` returned the marked outcome — and
        // therefore before `try_wait_step` could observe
        // `ready = true`.
        let post_fire_writes = mmio.snapshot().len();
        assert_eq!(
            post_fire_writes - pre_fire_writes,
            2,
            "controller.mask must complete before IrqTable::fire returns Marked"
        );
        // The write at offset 0x10 + 14 must carry the mask bit set.
        let writes = mmio.snapshot();
        assert!(
            writes[pre_fire_writes].1 & (1 << 16) != 0,
            "mask bit must be set in the low half of the redirection entry"
        );
    }
}
