//! riscv64 external-interrupt (PLIC) dispatch wiring (`plans/NETWORK.md`
//! N4e-riscv64).
//!
//! The production boot path runs [`tairix_arch_riscv64::trap::install_trap_vector`]
//! (the synchronous-trap half), which routes the `ecall` syscall path and
//! catches faults but arms **no** interrupt source. This module adds the
//! asynchronous half the interrupt-driven bootstrap-floor bring-up needs: it
//! builds the single-context S-mode PLIC controller over the
//! device-tree-discovered register base + source count ([`record_plic`],
//! called from the boot path where the firmware tree is in hand), publishes
//! it alongside the kernel-core [`IrqTable`], installs the S-mode
//! external-interrupt dispatcher (claim → [`IrqTable::fire`] → complete), and
//! enables `sie.SEIE` — leaving `sstatus.SIE` to the dispatch loop's
//! [`tairix_arch_riscv64::trap::set_supervisor_interrupts`] so no interrupt
//! is *taken* until the scheduler runs a task.
//!
//! It is the riscv64 analogue of the aarch64
//! [`crate::aarch64::gic_irq::install_device_irq_dispatch`], but far simpler:
//! the bare PLIC, no MSI controller and no composite fan-out (the `virt`
//! board has none). The in-kernel root-unlock kthread ([`crate::riscv64::root_unlock`])
//! binds and blocks on a device's line through [`published_irq_table`] and
//! [`plic_controller`], exactly as the aarch64 kthread does through its own
//! published table.

use alloc::boxed::Box;

use tairix_arch_riscv64::plic::{s_mode_context, Plic, PlicController, VolatilePlicMmio};
use tairix_arch_riscv64::{halt_current_hart, trap};
use tairix_kernel_core::IrqRouting;
use tairix_kernel_irq::{IrqController, IrqTable};
use tairix_sync::once::OnceCell;

use crate::riscv64_plic_irq::PlicIrqController;

/// Set-once PLIC parameters discovered from the firmware device tree:
/// `(register base, riscv,ndev source count)`. Recorded by the boot path
/// ([`record_plic`]) while the tree is in hand, and read by
/// [`install_dispatch`] to build the controller (which has no device tree).
static PLIC_INFO: OnceCell<(u64, u32)> = OnceCell::new();

/// Set-once slot for the kernel-core [`IrqTable`] the external-interrupt
/// dispatcher fires into. The in-kernel root-unlock kthread binds its
/// device line on **this** table (the one [`production_external_dispatch`]
/// fires into), reached through [`published_irq_table`].
static IRQ_TABLE_SLOT: OnceCell<&'static IrqTable> = OnceCell::new();

/// Set-once slot for the `'static` PLIC controller the dispatcher
/// claims/masks/completes through and the park path re-arms through.
static PLIC_CONTROLLER: OnceCell<&'static PlicIrqController<VolatilePlicMmio>> = OnceCell::new();

/// Record the device-tree-discovered PLIC register base and `riscv,ndev`
/// source count for [`install_dispatch`].
///
/// Called once from the boot path's hardware-tree seeding, where the
/// firmware device tree is parsed. Idempotent (set-once); a board with no
/// PLIC never calls it, and [`install_dispatch`] then wires no external-IRQ
/// dispatch (fail closed — interrupt-driven bring-up refuses rather than
/// parks forever on a line that can never fire).
pub fn record_plic(base: u64, ndev: u32) {
    let _ = PLIC_INFO.set((base, ndev));
}

/// The `'static` [`IrqTable`] published by [`install_dispatch`], or [`None`]
/// before it is published.
///
/// An in-kernel service kthread (the root-unlock kthread) that must bind and
/// block on a device's PLIC source binds on **this** table — the one
/// [`production_external_dispatch`] fires into — never a fresh table the trap
/// vector would never reach.
#[must_use]
pub fn published_irq_table() -> Option<&'static IrqTable> {
    IRQ_TABLE_SLOT.get().ok().flatten().copied()
}

/// The `'static` PLIC controller published by [`install_dispatch`], or
/// [`None`] before it is published (no PLIC discovered).
///
/// The root-unlock kthread arms its device's source and re-arms it after
/// each completion through this controller (the driver holds no PLIC
/// access), and hands it to its [`tairix_kernel_core::IrqParkWaiter`] as the
/// re-arm controller.
#[must_use]
pub fn plic_controller() -> Option<&'static PlicIrqController<VolatilePlicMmio>> {
    PLIC_CONTROLLER.get().ok().flatten().copied()
}

/// The S-mode external-interrupt dispatcher the arch trap handler invokes for
/// each acknowledged supervisor external interrupt.
///
/// Claims the pending PLIC source, forwards it to [`IrqTable::fire`] (which
/// masks the source before any waiter observes `ready` — mask-before-wake),
/// completes the claim, then wakes any parked `irq_wait` caller and latches a
/// reschedule so the woken work is dispatched. The device-level interrupt
/// acknowledgement is the *driver's* job (the transport's `InterruptACK`),
/// not the trap dispatch — mirroring the aarch64
/// [`crate::aarch64::gic_irq::production_device_irq_dispatch`], which does
/// `fire` only.
///
/// Wait-free and allocation-free, safe from interrupt context. A delivery
/// before the table/controller are published (impossible in production — the
/// core installs them before any line is armed) returns silently.
extern "C" fn production_external_dispatch() {
    let (Some(plic), Some(table)) = (plic_controller(), published_irq_table()) else {
        return;
    };
    let source = plic.claim();
    if source != 0 {
        let _ = table.fire(source, plic as &dyn IrqController);
        plic.complete(source);
        // Wake any `irq_wait` caller parked on the bound line (`fire` set the
        // per-line ready flag after masking — mask-before-wake holds), and
        // latch a reschedule so a task this interrupt woke is dispatched on
        // the next return-to-user preemption point (or, on an idle S-mode
        // hart, by the dispatch loop's wake drain). A spurious wake is
        // harmless — the waiter re-checks its own line and re-parks.
        tairix_kernel_core::irq_wake();
        tairix_kernel_core::note_preempt_tick(tairix_arch_riscv64::smp::current_hartid());
    }
}

/// Build the single-context S-mode PLIC controller from the discovered
/// [`PLIC_INFO`] and publish it into [`PLIC_CONTROLLER`], or return the
/// already-published one; [`None`] when no PLIC was discovered.
///
/// The one place the `'static` [`PlicIrqController`] is constructed, so the
/// kernel-core [`IrqTable`]'s masking seam ([`plic_routing`], run in
/// `Phase::Irq`) and the external-interrupt dispatch install
/// ([`install_dispatch`], run immediately after) share **one** controller
/// instance rather than each minting its own.
fn ensure_controller() -> Option<&'static PlicIrqController<VolatilePlicMmio>> {
    if let Some(controller) = PLIC_CONTROLLER.get().ok().flatten().copied() {
        return Some(controller);
    }
    let (base, ndev) = PLIC_INFO.get().ok().flatten().copied()?;
    let base = usize::try_from(base).ok()?;
    // SAFETY: `base` is the PLIC register-block base read from the firmware
    // device tree, identity-mapped (the boot path enabled the Sv39 identity
    // MMU before discovery) and exclusively the controller's to access on the
    // single-hart `virt` slice. `s_mode_context(0)` is the boot hart's
    // supervisor interrupt context.
    let controller = PlicIrqController::new(PlicController::new(
        Plic::new(unsafe { VolatilePlicMmio::new(base) }, s_mode_context(0)),
        ndev,
    ));
    // Boot-leaked to `'static`: the controller is shared for the life of the
    // system by the trap dispatcher, the root-unlock kthread, and every
    // interrupt-driven driver's park path (kernel state is never freed).
    let controller: &'static PlicIrqController<VolatilePlicMmio> = Box::leak(Box::new(controller));
    match PLIC_CONTROLLER.set(controller) {
        Ok(()) => Some(controller),
        // A concurrent publisher won the set-once race (not possible on the
        // single-hart boot slice, but fail safe): use the winner.
        Err(_) => PLIC_CONTROLLER.get().ok().flatten().copied(),
    }
}

/// The kernel-core IRQ routing this port hands the core in `Phase::Irq`: the
/// inclusive PLIC source ceiling as `max_line` (so the core [`IrqTable`] can
/// bind any discovered device source) and the shared [`PlicIrqController`] as
/// the mask/re-arm seam.
///
/// Without this the core would fall back to [`IrqRouting::unsupported`]
/// (`max_line = 0`), and every device-source `bind` — the root-unlock block
/// completion line, an autoloaded driver's line — would fail closed as
/// out-of-range. With no PLIC discovered it returns the unsupported routing,
/// and interrupt-driven bring-up fails closed rather than binding a line that
/// can never fire.
#[must_use]
pub fn plic_routing() -> IrqRouting {
    match (ensure_controller(), PLIC_INFO.get().ok().flatten().copied()) {
        (Some(controller), Some((_, ndev))) => IrqRouting {
            max_line: ndev,
            controller,
        },
        _ => IrqRouting::unsupported(),
    }
}

/// Publish `table`, install the external-interrupt dispatcher over the shared
/// PLIC controller ([`ensure_controller`]), and enable `sie.SEIE`.
///
/// Called once per boot from
/// [`RiscvBinArch::install_irq_dispatch`](crate::riscv64::boot::RiscvBinArch),
/// in the kernel-core `Irq` phase — immediately after `plic_routing` sized the
/// core [`IrqTable`] and built the controller, so this only publishes the
/// table and arms the trap path. A second publish (a stray re-call) fails
/// closed by halting the hart; the boot pipeline calls it exactly once.
///
/// With no PLIC discovered ([`PLIC_INFO`] empty — a bare part with no
/// interrupt controller, or an unreadable tree) it publishes the table but
/// wires no dispatch and returns: interrupt-driven bring-up then fails closed
/// (the root-unlock kthread refuses rather than parking forever on a line that
/// can never fire).
pub fn install_dispatch(table: &'static IrqTable) {
    if IRQ_TABLE_SLOT.set(table).is_err() {
        halt_current_hart();
    }
    // Reuse the one controller `plic_routing` already built and published in
    // `Phase::Irq` (or build it now if this port ever installs without a
    // routing step); no PLIC discovered leaves the dispatch unwired.
    let Some(_controller) = ensure_controller() else {
        return;
    };
    if trap::set_trap_dispatch(production_external_dispatch).is_err() {
        halt_current_hart();
    }
    // Enable supervisor external interrupts. The production boot ran
    // `install_trap_vector` (not `init_traps`), so `sie.SEIE` is not yet set;
    // `sstatus.SIE` is toggled by the dispatch loop's `set_device_irqs`, so
    // no interrupt is *taken* until the scheduler runs a task.
    //
    // SAFETY: the trap vector is installed (boot `enable_mmu_and_vectors`) and
    // the dispatcher is published (above), so a taken external interrupt
    // reaches a valid handler; `csrs sie` sets only `SIE_SEIE`, with no memory
    // side effects.
    unsafe {
        core::arch::asm!("csrs sie, {}", in(reg) trap::SIE_SEIE, options(nomem, nostack));
    }
}
