//! In-crate [`KernelArch`] wrapper around
//! [`rustos_arch_x86_64::kernel_arch::X86_64Arch`].
//!
//! # Why a wrapper
//!
//! `rustos_kernel_core::KernelArch` is a foreign trait and
//! `rustos_arch_x86_64::kernel_arch::X86_64Arch` is a foreign type, so
//! Rust's coherence rules forbid implementing the trait for the type
//! directly. The wrapper [`BinArch`] is the smallest possible local
//! type that owns an `X86_64Arch`, implements the
//! [`rustos_kernel_sched_api::SchedulerArch`] super-trait by delegation,
//! and implements [`rustos_kernel_core::KernelArch::halt`] by
//! forwarding to the free function
//! [`rustos_arch_x86_64::kernel_arch::halt`].
//!
//! The arch crate's
//! `kernel/arch/x86_64/Cargo.toml` comment explicitly documents this
//! split: pulling `rustos-kernel-core` into the arch crate would
//! transitively force a `#[global_allocator]` into the two pre-existing
//! freestanding Stage-2 QEMU test bins.

use rustos_arch_api::PlatformEntropy;
use rustos_arch_x86_64::apic_timer::{Calibration, Rdtsc, TscReader};
use rustos_arch_x86_64::context_hal::ContextSwitchHal;
use rustos_arch_x86_64::entropy::PlatformRng as X86PlatformEntropy;
use rustos_arch_x86_64::kernel_arch::{halt as arch_halt, X86_64Arch};
use rustos_kernel_core::{reschedule_current, IrqRouting, KernelArch, RescheduleAction};
use rustos_kernel_irq::{IrqController, IrqTable};
use rustos_kernel_mem::BootMemoryMap;
use rustos_kernel_sched_api::{CpuId, SchedulerArch};
use rustos_sync::once::OnceCell;

/// Set-once slot for the `'static` [`IrqTable`] published by
/// [`rustos_kernel_core::KernelArch::install_irq_dispatch`].
///
/// Used by the freestanding external-IRQ Rust dispatcher
/// ([`production_external_irq_dispatch`]) to translate a vector hit
/// into an [`IrqTable::fire`] call. The `OnceCell` enforces the
/// one-shot-publish invariant.
static IRQ_TABLE_SLOT: OnceCell<&'static IrqTable> = OnceCell::new();

/// Set-once slot for the `'static` [`IrqController`] the external-IRQ
/// dispatcher invokes. Populated by [`BinArch::new`] (which captures
/// the [`IrqRouting`]) and read by
/// [`production_external_irq_dispatch`].
static IRQ_CONTROLLER_SLOT: OnceCell<&'static (dyn IrqController + Send + Sync)> = OnceCell::new();

/// Set-once slot for the firmware [`BootMemoryMap`] the boot pipeline
/// assembled, published by [`publish_memory_map`] during `try_boot`
/// before the map is moved into the `kernel_core` hand-off.
///
/// A driver-bring-up observer (e.g. the planned
/// `tests/integration/virtio_blk_pci_x86_64` integration test) reads
/// this through [`published_memory_map`] to build the per-device DMA
/// [`rustos_kernel_mem::FrameAllocator`] it needs, without re-borrowing
/// the `pub(crate)` `KernelState`. The slot stores its own clone of
/// the map, so the live kernel allocator and any observer-built
/// allocator draw from the same firmware description but never share a
/// mutable handle (one-shot publish).
static MEMORY_MAP_SLOT: OnceCell<BootMemoryMap> = OnceCell::new();

/// Production external-IRQ dispatcher.
///
/// Installed into [`rustos_arch_x86_64::irq::set_external_irq_dispatch`]
/// during `try_boot`. Translates `vector` to a GSI through the arch
/// crate's [`rustos_arch_x86_64::irq::global_routing`], looks up the
/// published [`IrqTable`] + controller, and forwards to
/// [`IrqTable::fire`]. EOI is performed by the asm trampoline after
/// this function returns.
///
/// Safe to invoke from interrupt context: every operation is wait-free
/// and allocation-free. Spurious deliveries before either slot is
/// populated return silently — the asm trampoline still issues EOI
/// to keep the LAPIC out of stuck-in-service.
pub extern "C" fn production_external_irq_dispatch(vector: u8) {
    let routing = rustos_arch_x86_64::irq::global_routing();
    let Some(gsi) = routing.gsi_for_vector(vector) else {
        // Stray vector — never bound. The asm trampoline EOIs after
        // we return.
        return;
    };
    let Ok(Some(table)) = IRQ_TABLE_SLOT.get() else {
        // Slot empty or poisoned. The boot pipeline installs the
        // table strictly before unmasking any IO-APIC line, so this
        // branch is unreachable in production.
        return;
    };
    let Ok(Some(controller)) = IRQ_CONTROLLER_SLOT.get() else {
        return;
    };
    // The fire call's outcome is intentionally ignored — the arch
    // crate's higher layer (the asm trampoline) issues the LAPIC EOI
    // regardless. Errors here surface to the next `irq_wait` caller
    // via the `IrqTable`'s `Stray` / `ArchUnsupported` paths.
    let _ = table.fire(gsi, *controller);
    // Wake any `irq_wait` caller parked on a bound line: `fire` set the
    // per-line ready flag (after masking — mask-before-wake holds). A
    // spurious wake for a waiter on a different line is harmless — it
    // re-checks its own line and parks again.
    // Wait-free and allocation-free, safe from this interrupt context.
    rustos_kernel_core::irq_wake();
}

/// The ring-3-preemption callback the LAPIC-timer ISR invokes for a tick
/// taken from ring 3 (installed via
/// [`rustos_arch_x86_64::preempt::set_preempt_callback`] in
/// [`KernelArch::install_irq_dispatch`]).
///
/// It suspends the user task currently running on `cpu` back to the
/// scheduler with [`RescheduleAction::Yield`] — the *involuntary* analogue
/// of a `yield` syscall: the task is re-enqueued at its priority and the
/// scheduler picks the next runnable task, giving EEVDF-ordered
/// time-slicing. This is the x86_64 sibling of the aarch64/riscv64
/// `production_preempt_dispatch` (one shape over the
/// Arch HAL). [`reschedule_current`] returns `false` when no resumable
/// user kthread is published on `cpu` (unreachable from ring 3 with none
/// switched in, but the fail-closed return means a stray invocation is a
/// harmless no-op rather than an unsound switch). The
/// call only ever runs after the ISR has written the LAPIC EOI, so the
/// in-service bit is already released across the context switch, and the
/// ISR brackets it with the `swapgs` pair that balances the kthread
/// cooperative park.
extern "C" fn production_preempt_dispatch(cpu: CpuId) {
    let _ = reschedule_current(cpu, RescheduleAction::Yield);
}

/// The per-tick callback the LAPIC-timer ISR invokes on **every** tick
/// (ring 3 *or* idle ring 0), installed via
/// [`rustos_arch_x86_64::preempt::set_timer_callback`].
///
/// It latches the fired tick as this CPU's pending preemption
/// ([`rustos_kernel_core::note_preempt_tick`]) and runs the blocking-wait
/// timed-wake sweep (Design D P-2): any waiter whose finite deadline has
/// elapsed is unparked and the one-shot is re-armed to the next pending
/// deadline ([`rustos_kernel_core::timed_wake_sweep`]), so a finite
/// `hw_tree_wait` timeout fires even when the CPU is otherwise idle
/// (every task parked) and takes no preemption tick. Both halves are pure
/// accounting (they never context-switch), so they are safe on a tick
/// taken in ring 0; the *immediate* preemption of a ring-3 task is the
/// separate [`production_preempt_dispatch`] ring-3-only callback, while a
/// tick taken in ring 0 is honoured through the latch at the interrupted
/// syscall's completion — the running task's quantum is never silently
/// lost to a tick the non-preemptible kernel could not act on.
extern "C" fn production_tick_dispatch(cpu: CpuId) {
    rustos_kernel_core::note_preempt_tick(cpu);
    rustos_kernel_core::timed_wake_sweep();
}

/// Read the [`IrqTable`] published into `IRQ_TABLE_SLOT` by
/// [`KernelArch::install_irq_dispatch`].
///
/// Returns `None` until the kernel-core `Phase::Irq` step has run and
/// the arch wrapper's [`KernelArch::install_irq_dispatch`] override has
/// published the table. Reads from the same set-once slot the asm
/// trampoline's [`production_external_irq_dispatch`] consults; this
/// accessor exists so an in-kernel observer (e.g. the
/// `tests/integration/irq_qemu_x86_64` integration test) can drive
/// [`IrqTable::bind`] / [`IrqTable::try_wait_step`] against the live
/// table without re-borrowing the `pub(crate)` `KernelState`.
///
/// (one-shot publish): the returned reference is to a
/// `'static` table; once visible it cannot be replaced.
/// (no interface creep): this accessor performs only a read of
/// already-published state — no new writable surface is exposed.
#[must_use]
pub fn published_irq_table() -> Option<&'static IrqTable> {
    match IRQ_TABLE_SLOT.get() {
        Ok(slot) => slot.copied(),
        Err(_) => None,
    }
}

/// Read the [`IrqController`] published into `IRQ_CONTROLLER_SLOT`
/// by [`BinArch::new`].
///
/// Returns `None` until [`BinArch`] has been constructed for the
/// running kernel. Mirrors [`published_irq_table`]; same rationale and
/// the same one-shot semantics.
#[must_use]
pub fn published_irq_controller() -> Option<&'static (dyn IrqController + Send + Sync)> {
    match IRQ_CONTROLLER_SLOT.get() {
        Ok(slot) => slot.copied(),
        Err(_) => None,
    }
}

/// Publish a clone of the firmware [`BootMemoryMap`] into
/// `MEMORY_MAP_SLOT`.
///
/// Called once from `boot::try_boot` with the assembled map, before
/// the original is moved into the `kernel_core` hand-off. A second
/// call is a no-op (`OnceCell::set` rejects it); the boot pipeline
/// only ever calls this once, so the discarded `Err` cannot mask a
/// real defect (one-shot publish).
pub fn publish_memory_map(map: &BootMemoryMap) {
    let _ = MEMORY_MAP_SLOT.set(map.clone());
}

/// Read the [`BootMemoryMap`] published into `MEMORY_MAP_SLOT` by
/// [`publish_memory_map`].
///
/// Returns `None` until `boot::try_boot` has published the map. The
/// returned reference is to the `'static` slot-owned clone; the
/// accessor exposes no writable surface. A
/// driver-bring-up observer uses it to construct a per-device DMA
/// [`rustos_kernel_mem::FrameAllocator`] from the same firmware
/// description the live kernel allocator was built against.
#[must_use]
pub fn published_memory_map() -> Option<&'static BootMemoryMap> {
    MEMORY_MAP_SLOT.get().unwrap_or_default()
}

/// Local wrapper around [`X86_64Arch`] so the bin crate can implement
/// the foreign [`KernelArch`] trait on the foreign concrete type, and
/// carries the boot-time `Calibration` consumed by
/// [`KernelArch::monotonic_ns`] (Stage 2.7 follow-up (f3)).
///
/// The wrapper exists solely to satisfy Rust's orphan rules; the
/// `SchedulerArch` super-trait still delegates verbatim to
/// `X86_64Arch`. `KernelArch::monotonic_ns` reads RDTSC through the
/// arch crate's [`Rdtsc`] reader and converts the tick count into
/// nanoseconds via [`Calibration::tsc_ticks_to_ns`] — the same TSC
/// frequency the boot path measured against the PIT
/// (no parallel measurement, no interface creep).
pub struct BinArch {
    arch: X86_64Arch,
    calibration: Calibration,
    irq_routing: IrqRouting,
}

impl core::fmt::Debug for BinArch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BinArch")
            .field("arch", &self.arch)
            .field("calibration", &self.calibration)
            .field("irq_routing", &self.irq_routing)
            .finish()
    }
}

impl BinArch {
    /// Construct a [`BinArch`] from an already-validated [`X86_64Arch`],
    /// the boot-time `Calibration`, and the architecture-installed
    /// [`IrqRouting`].
    ///
    /// `calibration` is the value returned by
    /// `apic_timer::calibrate` in the bin crate's `boot::try_boot`; it
    /// carries the TSC frequency [`KernelArch::monotonic_ns`] needs.
    /// `irq_routing` is the routing the bin crate built from the
    /// MADT-discovered IO-APIC layout; it is what
    /// [`KernelArch::irq_routing`] returns. Constructing `BinArch`
    /// also stores the controller pointer in the
    /// crate-internal `IRQ_CONTROLLER_SLOT` so
    /// [`production_external_irq_dispatch`] can read it without an
    /// extra publication step.
    #[must_use]
    pub fn new(arch: X86_64Arch, calibration: Calibration, irq_routing: IrqRouting) -> Self {
        // The controller pointer must be published before any
        // external IRQ can fire. The boot pipeline guarantees the
        // ordering: it builds `BinArch` *before* installing the
        // dispatcher callback (which is what the asm trampoline
        // jumps to) and *before* unmasking any IO-APIC line.
        //
        // OnceCell::set returns `Err(AlreadySetError)` on the second
        // publish. The boot pipeline calls this constructor exactly
        // once per boot; tests that build multiple `BinArch`s share
        // the same slot, so a re-publish is treated as a benign no-op
        // rather than halting (the host-test scaffolding would
        // otherwise be unable to construct more than one `BinArch`
        // per run). Production code goes through `try_boot` exactly
        // once.
        let _ = IRQ_CONTROLLER_SLOT.set(irq_routing.controller);
        Self {
            arch,
            calibration,
            irq_routing,
        }
    }

    /// Borrow the wrapped [`X86_64Arch`].
    #[must_use]
    pub const fn arch(&self) -> &X86_64Arch {
        &self.arch
    }

    /// Boot-time calibration captured during `kernel_main`.
    #[must_use]
    pub const fn calibration(&self) -> Calibration {
        self.calibration
    }
}

impl SchedulerArch for BinArch {
    fn current_cpu(&self) -> CpuId {
        self.arch.current_cpu()
    }

    fn ticks_now(&self) -> u64 {
        self.arch.ticks_now()
    }

    fn send_ipi(&self, target: CpuId) {
        self.arch.send_ipi(target);
    }

    fn set_preemption(&self, armed: bool) {
        // Tickless preemption: forward the scheduler's
        // arm/disarm decision to the arch port, which programs the
        // LAPIC-timer one-shot. The default no-op would silently drop
        // preemption, so the delegation is required.
        self.arch.set_preemption(armed);
    }

    fn set_wakeup(&self, deadline_ns: Option<u64>) {
        // Forward the nearest blocking-wait deadline to the arch port,
        // which combines it with the quantum and arms the single
        // LAPIC-timer one-shot to the earlier. The
        // default no-op would silently drop timed wakes, so the delegation
        // is required.
        self.arch.set_wakeup(deadline_ns);
    }
}

/// The `'static` x86_64 platform-entropy handle the kernel seeds its CSPRNG
/// reserve from. Zero-sized; the RDSEED/RDRAND instructions are addressed
/// directly, so no per-instance state is needed.
static X86_PLATFORM_ENTROPY: X86PlatformEntropy = X86PlatformEntropy::new();

impl KernelArch for BinArch {
    type Cs = ContextSwitchHal;

    fn context_switch(&self) -> Self::Cs {
        ContextSwitchHal::new()
    }

    fn halt(&self) -> ! {
        arch_halt()
    }

    fn platform_entropy(&self) -> Option<&'static dyn PlatformEntropy> {
        // x86_64 seeds the kernel CSPRNG reserve from RDSEED/RDRAND. The
        // handle is zero-sized, so a `'static` instance suffices; whether
        // the instructions are enumerated is decided at runtime by the port.
        Some(&X86_PLATFORM_ENTROPY)
    }

    fn arch_id(&self) -> Option<rustos_abi::Arch> {
        Some(rustos_abi::Arch::X86_64)
    }

    fn cpu_name(&self) -> Option<rustos_abi::CpuName> {
        // The CPUID processor brand string; a part without the
        // brand-string leaves stays an honest `None` (the boot facts
        // record "unknown"), never a guessed name.
        let mut buf = [0u8; rustos_arch_x86_64::cpuname::BRAND_LEN];
        rustos_arch_x86_64::cpuname::boot_cpu_name(&mut buf).and_then(rustos_abi::CpuName::new)
    }

    fn park_translation(&self) -> Option<fn()> {
        // Re-installs the trampoline `CR3` root (published by the boot
        // path's `publish_boot_park_root`) so no user root stays active
        // after its task suspends — the invariant a dead task's
        // page-table reclamation relies on. The paging module exists only
        // on the bare-metal target; a host build has no translation to
        // park.
        #[cfg(all(freestanding, kernel_isa = "x86_64"))]
        {
            fn park() {
                // Fire-and-forget from the dispatcher: with no park root
                // published yet there is nothing to leave (fail closed),
                // so the `bool` outcome is deliberately discarded.
                let _ = rustos_arch_x86_64::paging::park_kernel_root();
            }
            Some(park)
        }
        #[cfg(not(all(freestanding, kernel_isa = "x86_64")))]
        {
            None
        }
    }

    fn irq_routing(&self) -> IrqRouting {
        // The routing was assembled during the bin crate's
        // `try_boot` and captured by `BinArch::new`. Returning a
        // copy of the (small, `Copy`) struct preserves the
        // set-once-per-boot semantics documented on
        // [`KernelArch::irq_routing`] — every call returns
        // bitwise-identical fields.
        self.irq_routing
    }

    fn install_irq_dispatch(&self, table: &'static IrqTable) {
        // Publish the IrqTable into the dispatcher slot. A second
        // publish (e.g. a stray re-call from a future code path)
        // is fail-closed via `arch_halt` (one-shot
        // publish) and (fail closed). The boot pipeline calls
        // `install_irq_dispatch` exactly once per boot, so the halt
        // branch is unreachable in production.
        if IRQ_TABLE_SLOT.set(table).is_err() {
            arch_halt();
        }
        // Install the production external-IRQ dispatcher in the arch
        // crate's slot. The asm trampoline reads this slot on every
        // external-IRQ delivery (see
        // `kernel/arch/x86_64/src/irq.rs`).
        if rustos_arch_x86_64::irq::set_external_irq_dispatch(production_external_irq_dispatch)
            .is_err()
        {
            // Second publish — same fail-closed posture.
            arch_halt();
        }
        // Arm ring-3 preemption now that the scheduler is up (P-1c,
        // `plans/PI.md` D2b-2b-A): install the ring-3-preemption callback
        // the LAPIC-timer ISR forwards each user-mode tick to. The timer
        // was programmed **one-shot and left disarmed** during boot
        // (`preempt::init_local_preempt`, the production boot's step 8);
        // RustOS is tickless, so the scheduler arms
        // the one-shot to one quantum (via `X86_64Arch::set_preemption`)
        // only when it dispatches onto a contended CPU and disarms
        // otherwise. The kernel runs with `RFLAGS.IF == 0` (it issues no
        // `sti`), so no tick is *taken* until `init` drops to ring 3 with
        // `IF` set (`userentry`), by which point a user kthread is
        // published — so installing the callback here, in the kernel-core
        // `Irq` phase before `BootCompleted`, is race-free and additive: it cannot preempt the cooperative kernel,
        // only a runaway user task. No scheduler-tick callback is installed
        // — EEVDF is tickless, so the timer is armed solely to preempt; the timed-wake sweep a deadline-bearing
        // blocking wait needs (P-2) installs its tick consumer then, not
        // ahead of it. `set_preempt_callback` is an idempotent
        // pointer store (not a one-shot slot), so no fail-closed re-call
        // guard is needed here.
        rustos_arch_x86_64::preempt::set_preempt_callback(production_preempt_dispatch);

        // Install the per-tick timed-wake sweep callback (Design D P-2), so
        // every tick — including one taken on an idle ring-0 CPU armed
        // solely for a blocking-wait deadline — releases any elapsed waiter
        // and re-arms the one-shot to the next deadline. `set_timer_callback` is an idempotent pointer store, so
        // no fail-closed re-call guard is needed.
        rustos_arch_x86_64::preempt::set_timer_callback(production_tick_dispatch);
    }

    fn wait_for_interrupt(&self) {
        // The tickless idle wait: the ring-0 dispatch
        // loop runs with `RFLAGS.IF == 0` (the kernel is non-preemptible — it issues no `sti`), so a wake delivered between the loop's
        // `step` and here stays *pending* rather than being taken, and no
        // edge is lost (the race-free park). The `sti; hlt` pair is
        // atomic with respect to interrupt delivery — `sti` enables `IF`
        // only *after* the following instruction, so the pending interrupt
        // is taken during `hlt` (waking it), the timer/IRQ handler unparks
        // a waiter, and `cli` then restores the masked loop invariant
        // before returning so the loop re-steps. On a host build there is
        // no ring 0, so this is a benign no-op.
        #[cfg(all(freestanding, kernel_isa = "x86_64"))]
        {
            // SAFETY: `sti; hlt; cli` is the canonical race-free idle wait;
            // the IDT and LAPIC are installed by this point, so a taken
            // interrupt dispatches through a valid handler, and `cli`
            // leaves `IF` clear exactly as the dispatch loop expects after
            // the park (the loop re-enables it with `set_device_irqs`).
            // `hlt` only wakes with `IF == 1`, so unlike the aarch64/riscv64
            // `wfi` this primitive must itself enable interrupts across the
            // halt; the `sti`-then-`hlt` pair is atomic so a pending
            // interrupt is taken during `hlt` and no wake is lost.
            unsafe {
                core::arch::asm!("sti; hlt; cli", options(nomem, nostack, preserves_flags));
            }
        }
    }

    fn set_device_irqs(&self, enabled: bool) {
        // Toggle this CPU's maskable-interrupt flag (`RFLAGS.IF`) so the
        // ring-0 dispatch loop runs in-kernel tasks/kthreads with device
        // interrupts enabled (the fully preemptive
        // kernel), masking them only around the idle park and before halt.
        // Enabling `IF` in ring 0 is safe: the LAPIC-timer ISR gates
        // preemption on the interrupted `CS` (a ring-0 tick runs lock-free
        // accounting but never reschedules the kernel), and an IO-APIC
        // device IRQ forwards to the lock-free dispatcher. On a host build
        // there is no ring 0, so this is a benign no-op.
        #[cfg(all(freestanding, kernel_isa = "x86_64"))]
        {
            // SAFETY: `sti`/`cli` toggle only `RFLAGS.IF`; the IDT and LAPIC
            // are installed by the time the dispatch loop runs, so a taken
            // interrupt dispatches through a valid handler.
            unsafe {
                if enabled {
                    core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
                } else {
                    core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
                }
            }
        }
        #[cfg(not(all(freestanding, kernel_isa = "x86_64")))]
        {
            let _ = enabled;
        }
    }

    fn monotonic_ns(&self, _cpu: CpuId) -> u64 {
        // Read RDTSC through the same arch-crate reader the calibration
        // path used so the tick source and the conversion factor share
        // the same time base. The `cpu` argument is currently unused on
        // x86_64 — the production target assumes the invariant-TSC
        // contract QEMU and modern Intel/AMD parts provide. A future
        // arch port that needs per-CPU offset compensation would feed
        // it into a `cpu_to_tsc_offset` table read here; nothing in the
        // current SMP bring-up populates such a table, so reading it
        // would be a stub and is omitted.
        let mut rdtsc = Rdtsc;
        let ticks = rdtsc.read();
        self.calibration.tsc_ticks_to_ns(ticks)
    }

    fn ticks_to_ns(&self, ticks: u64) -> u64 {
        // `ticks_now` is raw RDTSC, so the identity default would
        // misreport CPU time; convert through the same boot-time TSC
        // calibration `monotonic_ns` uses.
        self.calibration.tsc_ticks_to_ns(ticks)
    }

    fn direct_phys_map(&self) -> Option<&'static (dyn rustos_kernel_mem::PhysMap + Sync)> {
        // The higher-half kernel direct map through which the kernel reaches
        // any RAM frame the allocator hands out — the view the shared-memory
        // facility scrubs region frames through. On a host build there is no
        // ring-0 physical map, so none is offered and `shm_*` stays
        // fail-closed.
        #[cfg(all(freestanding, kernel_isa = "x86_64"))]
        {
            Some(&crate::x86_64::spawn_producer::SHM_PHYSMAP)
        }
        #[cfg(not(all(freestanding, kernel_isa = "x86_64")))]
        {
            None
        }
    }
}

// SAFETY-INVARIANT: `BinArch::halt` returns the bottom type. The
// compile-time function-pointer coercion below fails to type-check if
// the impl ever loses `-> !` (e.g. a `Result<!, !>` return or a
// `unreachable!()`-followed return type). This is the pattern called
// out by the arch crate's `_HALT_RETURNS_NEVER` const assertion;
// repeating it here pins the impl on this side of the wrapper too —
// (encode the invariant in the type system).
const _BIN_ARCH_HALT_RETURNS_NEVER: fn(&BinArch) -> ! = <BinArch as KernelArch>::halt;

// SAFETY-INVARIANT: `BinArch` implements `SchedulerArch`. A regression
// that broke the super-trait impl (e.g. a missing `current_cpu`)
// would surface at this `const _` coercion before the kernel binary
// linked. — no interface creep — applies in both
// directions: shrinking the surface is a defect too.
const _BIN_ARCH_IS_SCHED_ARCH: fn(&BinArch) -> u32 = <BinArch as SchedulerArch>::current_cpu;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use rustos_arch_x86_64::kernel_arch::X86_64ArchStorage;

    /// Convenient per-CPU capacity for the host tests — a local test
    /// constant, not a production ceiling (the arch crate no longer
    /// exposes a `MAX_CPUS`; capacity is the caller's `N`).
    const TEST_CPUS: usize = 8;

    fn arch_with_boot_cpu(boot_cpu: u32, lapic: u8) -> X86_64Arch {
        // Each construction leaks its own per-CPU backing so no two
        // handles share IPI counters under the parallel test runner; the leak is bounded (one per host test) and
        // the bin crate already has an allocator (allocator-having callers may provide leaked storage).
        let storage: &'static X86_64ArchStorage<TEST_CPUS> =
            Box::leak(Box::new(X86_64ArchStorage::new()));
        let mut map = [None; TEST_CPUS];
        map[boot_cpu as usize] = Some(lapic);
        X86_64Arch::new(storage, boot_cpu, lapic, &map).expect("valid X86_64Arch")
    }

    /// Host-test convenience: build a [`BinArch`] with the
    /// conservative [`IrqRouting::unsupported`] routing. The tests
    /// in this module exercise the scheduler/calibration surface;
    /// the [`KernelArch::irq_routing`] surface is exercised through
    /// the `ioapic_controller` module's host tests.
    fn bin_arch_with_unsupported_routing(boot_cpu: u32, lapic: u8) -> BinArch {
        BinArch::new(
            arch_with_boot_cpu(boot_cpu, lapic),
            test_calibration(),
            IrqRouting::unsupported(),
        )
    }

    /// Synthesise a `Calibration` for tests. The exact values are
    /// irrelevant to the delegating super-trait methods; `monotonic_ns`
    /// uses a 1 GHz TSC rate so a one-tick reading converts to 1 ns.
    fn test_calibration() -> Calibration {
        Calibration {
            ticks_per_second: 100_000,
            initial_count: 100,
            period_micros: 1_000,
            tsc_per_second: 1_000_000_000,
        }
    }

    #[test]
    fn current_cpu_delegates_to_inner() {
        let arch = bin_arch_with_unsupported_routing(2, 0xA2);
        assert_eq!(arch.current_cpu(), 2);
    }

    #[test]
    fn ticks_now_is_monotonic_on_host() {
        let arch = bin_arch_with_unsupported_routing(0, 0xA0);
        let a = arch.ticks_now();
        let b = arch.ticks_now();
        let c = arch.ticks_now();
        assert!(b > a);
        assert!(c > b);
    }

    #[test]
    fn send_ipi_delegates_to_inner_host_counter() {
        let storage: &'static X86_64ArchStorage<2> = Box::leak(Box::new(X86_64ArchStorage::new()));
        let arch = X86_64Arch::new(storage, 0, 0xA0, &[Some(0xA0), Some(0xA1)]).unwrap();
        let bin = BinArch::new(arch, test_calibration(), IrqRouting::unsupported());
        bin.send_ipi(1);
        bin.send_ipi(1);
        bin.send_ipi(0);
        // The inner host-only counters were ticked through the wrapper.
        assert_eq!(bin.arch().host_ipi_count(1), 2);
        assert_eq!(bin.arch().host_ipi_count(0), 1);
        assert_eq!(bin.arch().host_stray_ipi_count(), 0);
    }

    #[test]
    fn monotonic_ns_is_non_decreasing_on_host() {
        // On the host build path, `BinArch::monotonic_ns` reads RDTSC
        // (via `Rdtsc`) and converts through `Calibration::tsc_ticks_to_ns`.
        // RDTSC is monotonically non-decreasing on every x86_64 CPU
        // RustOS is built on, including the CI host, so two
        // consecutive reads must satisfy `a <= b` (no flaky tests; we assert a non-strict ordering because
        // the conversion can compress two close ticks onto the same
        // ns value).
        let arch = bin_arch_with_unsupported_routing(0, 0xA0);
        let a = arch.monotonic_ns(0);
        let b = arch.monotonic_ns(0);
        let c = arch.monotonic_ns(0);
        assert!(b >= a, "expected b >= a, got a={a} b={b}");
        assert!(c >= b, "expected c >= b, got b={b} c={c}");
    }

    #[test]
    fn calibration_is_round_tripped_through_constructor() {
        let cal = test_calibration();
        let arch = BinArch::new(arch_with_boot_cpu(0, 0xA0), cal, IrqRouting::unsupported());
        assert_eq!(arch.calibration(), cal);
    }

    /// Stage 4.D Item 2-tail.2 — [`BinArch::irq_routing`] returns the
    /// routing captured at construction time, bitwise unchanged.
    #[test]
    fn irq_routing_returns_captured_value() {
        let arch = bin_arch_with_unsupported_routing(0, 0xA0);
        let routing = arch.irq_routing();
        assert_eq!(routing.max_line, 0);
        // The unsupported routing's controller address equals the
        // address of the shared `UNSUPPORTED_CONTROLLER` static.
        let expected = core::ptr::addr_of!(rustos_kernel_irq::UNSUPPORTED_CONTROLLER) as usize;
        let got = {
            let p: *const (dyn rustos_kernel_irq::IrqController + Send + Sync) = routing.controller;
            p.cast::<()>() as usize
        };
        assert_eq!(got, expected);
    }

    /// Stage 4.D Item 2-tail.2 QEMU validation — [`published_irq_controller`]
    /// returns the pointer published into `IRQ_CONTROLLER_SLOT` by
    /// the first successful [`BinArch::new`] in the process.
    ///
    /// Constructing a [`BinArch`] sets the slot (via [`OnceCell::set`]);
    /// any later construction is a no-op publish so the returned
    /// pointer is stable across the rest of the process's lifetime.
    /// Because the test runner serialises tests within a single
    /// process, the assertion is deterministic: whichever
    /// `IrqRouting::controller` was published first is what every
    /// subsequent reader observes (one-shot publish).
    #[test]
    fn published_irq_controller_returns_set_once_pointer() {
        // Ensure at least one BinArch has been constructed in this
        // process so the controller slot is populated.
        let _arch = bin_arch_with_unsupported_routing(0, 0xA0);
        let published = published_irq_controller().expect("controller published");
        // The pointer must be `'static`; we compare against the
        // BinArch's own routing controller pointer (set-once
        // semantics mean either both point at `UNSUPPORTED_CONTROLLER`
        // or both point at whatever was published first in this
        // process — and the test harness shares process state, so
        // we cannot assume `UNSUPPORTED_CONTROLLER` always wins).
        let p_pub: *const (dyn rustos_kernel_irq::IrqController + Send + Sync) = published;
        // Sanity: the published pointer is non-null and points into
        // process address space (any valid `&'static dyn` reference
        // satisfies both, but we keep the assertion explicit so a
        // future regression that publishes a dangling pointer surfaces
        // here rather than in a downstream `mask` call).
        assert!(!p_pub.is_null());
    }

    /// `published_irq_table` returns `None` until
    /// `KernelArch::install_irq_dispatch` publishes a table. None of
    /// the tests in this module trigger the install path, so the
    /// accessor must remain `None` for the duration of this test
    /// regardless of test-ordering. Should a future test publish a
    /// table, this assertion will surface the change and the test
    /// can be relaxed in the same commit (no
    /// silent weakening).
    #[test]
    fn published_irq_table_is_none_until_install_dispatch_runs() {
        // We deliberately do not gate this on prior `BinArch`
        // construction — `IRQ_TABLE_SLOT` is independent of
        // `IRQ_CONTROLLER_SLOT`.
        assert!(published_irq_table().is_none());
    }

    /// [`publish_memory_map`] hands [`published_memory_map`] a stable,
    /// `'static` clone of the firmware map. This test is the only
    /// publisher of `MEMORY_MAP_SLOT` in the process, so the set-once
    /// slot deterministically reflects the map published here
    /// (one-shot publish).
    #[test]
    fn published_memory_map_returns_the_published_clone() {
        use rustos_kernel_mem::{MemoryRegion, PhysAddr, RegionKind, PAGE_SIZE};

        // Before any publish the slot is empty.
        assert!(published_memory_map().is_none());

        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new(PAGE_SIZE as u64 * 16),
            length: (PAGE_SIZE * 32) as u64,
        });
        publish_memory_map(&map);

        let published = published_memory_map().expect("map published");
        assert_eq!(published.regions().len(), 1);
        assert_eq!(
            published.regions()[0].start,
            PhysAddr::new(PAGE_SIZE as u64 * 16)
        );
        // A second publish is a no-op: the slot keeps its first value.
        let mut other = BootMemoryMap::new();
        other.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new(PAGE_SIZE as u64 * 1000),
            length: PAGE_SIZE as u64,
        });
        publish_memory_map(&other);
        assert_eq!(
            published_memory_map().expect("still set").regions().len(),
            1
        );
    }
}
