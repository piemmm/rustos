//! Architecture handover types for `kernel/core`.
//!
//! The architecture port (Stage 3 of `PLAN.md`) builds a [`BootInfo`]
//! from whatever protocol the platform exposes (multiboot2, UEFI, DTB,
//! `wasm-bindgen`, …) and hands it to [`crate::kernel_main`]. The
//! struct is deliberately the *only* contract between the
//! architecture-neutral half of the kernel and an arch crate:
//! everything Stage 3 needs to plug in is reachable from here, and
//! nothing else is.
//!
//! # Stability
//!
//! `BootInfo` is part of the in-tree `abi-v1` surface:
//! the layout is frozen on release. Extensions ship as new versioned
//! types (no interface creep) — never as silent
//! field additions.
//!
//! # SAFETY-INVARIANTS
//!
//! Every invariant the arch port must uphold before calling
//! [`crate::kernel_main`] is enumerated on [`BootInfo`]'s field
//! documentation with a `// SAFETY-INVARIANT:` tag and re-asserted at
//! entry where feasible (see [`BootInfo::validate`]).

use alloc::sync::Arc;

use rustos_arch_api::{ContextSwitch, PlatformEntropy};

use crate::sched::{CpuId, SchedulerArch, SchedulerConfig};
use rustos_kernel_irq::{IrqController, IrqTable, UNSUPPORTED_CONTROLLER};
use rustos_kernel_mem::BootMemoryMap;
use rustos_log::{Level, Sink};

use crate::console::{ConsoleDevice, NO_CONSOLES};
use crate::dispatch_slot::DispatchCallbackSlot;
use crate::fs::{
    FilesystemService, LateIdentity, VolumeForest, VolumeService, NULL_FILESYSTEM,
    NULL_VOLUME_FOREST, NULL_VOLUME_SERVICE,
};
use crate::hwtree::{HwTreeSource, NULL_HW_TREE};
use crate::seat::{SeatRegistry, NULL_SEAT_REGISTRY};
use crate::spawn::{
    InitSpawn, ProcessSpawn, ProgramRegistry, EMPTY_PROGRAM_REGISTRY, NULL_PROCESS_SPAWN,
};
use crate::useradmin::{UsersAdmin, NULL_USERS_ADMIN};
use crate::users::{UsersDbSource, NULL_USERS_DB};

/// Architecture-neutral hook the kernel core needs from a Stage 3
/// arch port.
///
/// This trait is the *only* arch surface `kernel/core` reaches for.
/// Anything more elaborate (per-core timer programming, MMU primitives,
/// CPU control registers) lives in the arch crate itself and is not
/// part of the contract here.
///
/// Implementations must be both [`Send`] and [`Sync`] because the
/// kernel core stores them inside `Arc`s shared between every CPU.
///
/// # Required semantics
///
/// * [`Self::halt`] **must not return**: and the
///   Stage 2 deliverables, a panic or an unrecoverable init failure
///   parks the CPU forever and never silently resets. Real ports
///   typically loop on `hlt` / `wfi` / `wfe` with interrupts disabled.
/// * [`SchedulerArch::current_cpu`] returns the calling CPU's
///   identifier. Used by the panic handler when dumping context.
pub trait KernelArch: SchedulerArch {
    /// The Arch-HAL context-switch primitive (the
    /// "context switch" slice of the closed arch surface) this port
    /// exposes.
    ///
    /// `kernel/core` reaches it through [`Self::context_switch`] to run a
    /// user task as a *resumable kernel thread*: PID 1 (and every later
    /// spawned process) is admitted with [`crate::spawn_user_kthread`],
    /// whose work diverges into EL0 and whose syscall trap path suspends
    /// it back to the scheduler through the same [`ContextSwitch::switch`]
    /// (`plans/SPAWN.md` SP2). The handle is a zero-sized, `Copy` value on
    /// every port — the per-task state lives in the
    /// [`rustos_arch_api::TaskContext`] the runtime owns, not in the
    /// handle — so it is cheap to hand to the runtime by value.
    type Cs: ContextSwitch + Copy + Send + 'static;

    /// Return this port's [context-switch handle](Self::Cs).
    ///
    /// Called when admitting a user kthread so the runtime can seed the
    /// task's first kernel-stack frame and switch into/out of it
    /// (`plans/SPAWN.md` SP2). The returned handle is stateless and
    /// `Copy`, so a fresh value per call is equivalent to any other.
    fn context_switch(&self) -> Self::Cs;

    /// Park the calling CPU forever.
    ///
    /// Called by the panic handler and by [`crate::kernel_main`] after
    /// a fatal init failure. Implementations must mask interrupts and
    /// loop on the lowest-power instruction the platform offers (e.g.
    /// `hlt` on x86_64, `wfi` on aarch64/riscv64, `loop { yield }` on
    /// `wasm32`).
    ///
    /// # SAFETY-INVARIANT
    ///
    /// This function never returns. The `!` return type encodes the
    /// invariant at the type level; production arch ports must not
    /// circumvent it by using `loop {}` followed by `unreachable!()`
    /// — the compiler-enforced bottom type is the contract.
    fn halt(&self) -> !;

    /// Nanoseconds elapsed since the kernel began running, as observed
    /// by `cpu`.
    ///
    /// The contract is **monotonically non-decreasing per CPU**:
    /// consecutive calls on the same CPU must never produce a smaller
    /// value than a prior call on that CPU. Cross-CPU drift is
    /// permitted up to the platform's hardware skew (e.g. RDTSC sync
    /// across sockets); callers requiring a strictly global ordering
    /// must funnel reads through one CPU.
    ///
    /// There is **no default impl**: every arch port must opt in so an
    /// arch shipping a non-monotonic clock cannot silently leak that
    /// flaw into the `clock_get` syscall (fail
    /// closed). x86_64 wires this through `apic_timer::Calibration`'s
    /// TSC sample.
    ///
    /// `cpu` is the calling CPU's identifier — the same value
    /// [`SchedulerArch::current_cpu`] returns. Arch ports may use it
    /// to apply per-CPU TSC offset compensation; the contract does
    /// not require them to.
    fn monotonic_ns(&self, cpu: CpuId) -> u64;

    /// The Tier-1 architecture identity of this port, or `None` for a
    /// port that is not a shippable target (the host test arch).
    ///
    /// Consumed once at boot to mint the [`rustos_abi::BootFacts`] record
    /// the ungated `boot_facts_get` syscall reports. There is **no default
    /// impl**: every port must state its identity explicitly, so a new
    /// port cannot silently ship reporting another architecture's name
    /// (fail closed — a `None` leaves the boot facts uninstalled).
    fn arch_id(&self) -> Option<rustos_abi::Arch>;

    /// Convert a span measured in [`SchedulerArch::ticks_now`] units into
    /// nanoseconds.
    ///
    /// The scheduler accounts per-task CPU time in raw ticks so its
    /// dispatch hot path pays a subtraction, never a division; readers
    /// (the System Information process feed) convert at observation time
    /// through this hook. The default is the identity — correct only for
    /// a port whose tick already is one nanosecond (the wasm32 port and
    /// the host test arch). A port whose [`SchedulerArch::ticks_now`]
    /// returns raw counter ticks (`CNTPCT`, `RDTSC`, the `time` CSR) must
    /// override this with the same frequency its
    /// [`Self::monotonic_ns`] conversion uses, so the two clocks can
    /// never diverge.
    fn ticks_to_ns(&self, ticks: u64) -> u64 {
        ticks
    }

    /// The port's "park the calling CPU's translation regime on the
    /// permanent boot kernel root" primitive, or `None` for a port with
    /// no hardware user address spaces (the wasm32 sandbox, the host
    /// test arch).
    ///
    /// [`crate::kernel_main`] installs the returned hook set-once into
    /// the dispatcher, which calls it after every switch-back from a
    /// user task so no user space's page-table root remains a CPU's
    /// active translation once its task stops running — the invariant
    /// that makes a dead task's page-table reclamation (the live-space
    /// drop at reap) safe on SMP. A paging port returns its
    /// `paging::park_kernel_root` free function (a plain `fn`, so the
    /// hook captures nothing and is trivially `Send`).
    fn park_translation(&self) -> Option<fn()> {
        None
    }

    /// IRQ routing the architecture port has installed.
    ///
    /// Consulted by [`crate::kernel_main`] during the [`crate::Phase::Irq`]
    /// init step (between `Sched` and `Syscall`). The kernel
    /// constructs the [`rustos_kernel_irq::IrqTable`] with the
    /// returned `max_line` and threads the returned controller
    /// through every subsequent [`rustos_kernel_irq::IrqTable::fire`]
    /// call.
    ///
    /// # Contract
    ///
    /// * The returned [`IrqRouting`] must be **set-once per boot**.
    ///   Arch ports build the controller during their pre-`kernel_main`
    ///   wiring phase and hand the kernel core a stable
    ///   `'static`-lifetime reference; calling the method twice from
    ///   the same boot must return values that agree on `max_line`
    ///   and on the controller's identity. The kernel core does not
    ///   re-call after the `Irq` phase completes, but a stray re-call
    ///   from a future code path must not observe a different
    ///   controller (one-shot publish).
    /// * The `mask_line` bound must encompass every line the arch
    ///   port intends to expose: `IrqTable::bind` refuses
    ///   `line > max_line`, so the bound is the user-visible IRQ
    ///   surface.
    /// * The controller's [`IrqController::mask`] must be safe to call
    ///   from interrupt context (the production trap path invokes it
    ///   with interrupts disabled).
    ///
    /// # Default
    ///
    /// The default impl returns [`IrqRouting::unsupported`], which is
    /// the conservative fail-closed shape: `max_line = 0` so only
    /// line `0` can be bound, and every `mask` call returns
    /// [`rustos_kernel_irq::MaskError::Unsupported`] —
    /// [`rustos_kernel_irq::IrqTable::fire`] in turn surfaces
    /// [`rustos_kernel_irq::IrqError::ArchUnsupported`]. Arch ports
    /// without a programmable interrupt controller (`wasm32`, test
    /// harnesses) inherit this default; ports with one
    /// (`x86_64`, `aarch64`, `riscv64`) override it during their
    /// pre-`kernel_main` boot pipeline.
    #[must_use]
    fn irq_routing(&self) -> IrqRouting {
        IrqRouting::unsupported()
    }

    /// Hand the kernel core the architecture's MSI-allocation facility, if
    /// it has one, so the `msi_alloc` syscall can mint message-signalled
    /// interrupt vectors for PCI functions.
    ///
    /// Allocating an MSI vector and bringing the platform's MSI controller
    /// up is irreducibly architecture-specific (the BCM2711 root-complex MSI
    /// controller on the Pi 4, an IO-APIC/LAPIC MSI domain on x86_64), so —
    /// like [`Self::irq_routing`] — the port supplies the concrete producer
    /// and the kernel core installs it into the syscall handler. The
    /// reference must be `'static` (the port backs it with a `static` or a
    /// `Box::leak`'d allocation) and is read once, during
    /// [`crate::Phase::Syscall`].
    ///
    /// # Default
    ///
    /// The default returns [`None`]: a port with no MSI controller
    /// (`wasm32`, test harnesses, and ports that have not wired one) leaves
    /// the handler's fail-closed [`crate::devres::NullMsiAllocFacility`] in
    /// place, so `msi_alloc` returns [`rustos_kernel_irq`]-style
    /// `NotImplemented`.
    #[must_use]
    fn msi_alloc_facility(
        &self,
    ) -> Option<&'static (dyn crate::devres::MsiAllocFacility + 'static)> {
        None
    }

    /// Hand the kernel core the architecture's **direct physical map** — the
    /// kernel-privileged view through which it can read and write any RAM
    /// frame by physical address — so the shared-memory facility can scrub a
    /// region's frames on allocation and on free (`plans/USB.md`; the
    /// zero-on-free guarantee).
    ///
    /// The map is irreducibly architecture-specific: the aarch64 / riscv64
    /// ports identity-map RAM (`virtual == physical`), while x86_64 maps it
    /// into the higher half at a fixed offset, so — like
    /// [`Self::msi_alloc_facility`] — the port supplies the concrete map and
    /// the kernel core installs it. The reference must be `'static` and is
    /// read once, during [`crate::Phase::Syscall`].
    ///
    /// # Default
    ///
    /// The default returns [`None`]: a port that wires no direct map (the
    /// `TestArch` mock, `wasm32`) leaves the handler's fail-closed
    /// [`crate::devres::NullSharedMemFacility`] in place, so `shm_*` return
    /// [`rustos_abi::Errno::NotImplemented`].
    #[must_use]
    fn direct_phys_map(&self) -> Option<&'static (dyn rustos_kernel_mem::PhysMap + Sync)> {
        None
    }

    /// Hand the kernel core the architecture's **platform entropy source** —
    /// the per-port hardware random-number handle (x86 `RDSEED`/`RDRAND`,
    /// ARMv8.5 `RNDR`, the RISC-V `Zkr` `seed` CSR) — so the boot path can
    /// seed the kernel CSPRNG output reserve from it. Without it the reserve
    /// stays unseeded and `random_get` fails closed.
    ///
    /// The source is irreducibly architecture-specific (only the port can
    /// issue the instruction), so — like [`Self::direct_phys_map`] — the port
    /// supplies the handle and the kernel core conditions its output through
    /// the `lib/rng` DRBG before any caller sees it. The reference must be
    /// `'static` and is read once, at boot.
    ///
    /// # Default
    ///
    /// The default returns [`None`]: a port that wires no source (the
    /// `TestArch` mock) leaves the reserve unseeded, so `random_get` keeps
    /// failing closed with [`rustos_abi::Errno::EntropyNotReady`].
    #[must_use]
    fn platform_entropy(&self) -> Option<&'static dyn PlatformEntropy> {
        None
    }

    /// Hand the architecture port a `'static` reference to the
    /// kernel-wide [`IrqTable`] once [`crate::Phase::Irq`] has
    /// constructed it.
    ///
    /// The arch port's external-IRQ trap dispatcher needs this
    /// reference to translate a vector-level hit (e.g. an IO-APIC pin
    /// firing) to the architecture-neutral
    /// [`IrqTable::fire`] call. Because the table is built inside
    /// `kernel_main` (it cannot exist before the arch port hands the
    /// kernel a `max_line` via [`Self::irq_routing`]), the hook is
    /// the only kernel-core → arch publication channel for the
    /// reference.
    ///
    /// # Contract
    ///
    /// * Called **exactly once per boot**, immediately after the
    ///   [`Phase::Irq`] ready event. A second call from any future
    ///   code path is a defect (one-shot publish);
    ///   real arch ports fail-closed on the second call by halting.
    /// * The `table` reference outlives the running kernel because
    ///   `kernel_main` `Box::leak`s the crate-internal `KernelState`
    ///   wrapping it.
    /// * The default impl is a no-op so arch ports without an
    ///   external-IRQ trap dispatcher (the `TestArch` mock,
    ///   `wasm32`) inherit no work.
    ///
    /// [`Phase::Irq`]: crate::Phase::Irq
    fn install_irq_dispatch(&self, table: &'static IrqTable) {
        // Default: no-op. The argument is consumed so the trait
        // method has a concrete signature compilers can monomorphise
        // through.
        let _ = table;
    }

    /// Park the calling CPU until the next interrupt, then return.
    ///
    /// Unlike [`Self::halt`] (which never returns), this is the *idle
    /// wait*: when the dispatch loop finds no runnable task but live tasks
    /// are still **parked** (e.g. a perpetual service blocked in a
    /// blocking-wait syscall), the CPU sleeps on the lowest-power
    /// wait-for-event instruction until the armed one-shot
    /// ([`SchedulerArch::set_wakeup`]) or a device IRQ fires.
    ///
    /// The loop calls this **with device IRQs masked** (it has just called
    /// [`Self::set_device_irqs(false)`](Self::set_device_irqs) and drained
    /// any already-flagged wake) to close the park/wake race: a `wfi`-class
    /// instruction wakes on a *pending-but-masked* interrupt, so an IRQ that
    /// asserts after the drain but before this call still wakes the CPU
    /// rather than being lost (no lost wake-up). The
    /// loop then re-enables IRQs ([`Self::set_device_irqs(true)`](Self::set_device_irqs)),
    /// at which point the pending interrupt is *taken* and its lock-free
    /// handler flags the deferred wake the next
    /// [`drain_pending_wakes`](crate::waitq::drain_pending_wakes) consumes.
    ///
    /// # Contract
    ///
    /// * It **must** return after the next interrupt becomes pending; it
    ///   must never spin or busy-wait.
    /// * It must leave the CPU's interrupt-mask state exactly as it found
    ///   it (masked) — re-enabling is the loop's job, not this method's.
    /// * It must never panic.
    ///
    /// # Default
    ///
    /// A no-op so the `TestArch` mock and any port without an idle-wait
    /// primitive inherit a benign (busy) re-step rather than blocking; a
    /// real port overrides it with its `wfi` / `hlt` / host-yield. The
    /// no-op default never deadlocks the loop because the loop only calls
    /// it while at least one task is live and will re-evaluate after every
    /// return.
    fn wait_for_interrupt(&self) {}

    /// Enable (`enabled = true`) or mask (`enabled = false`) device-IRQ
    /// taking at the processing element for the calling CPU.
    ///
    /// This is what makes RustOS a **fully preemptive** kernel: the scheduler dispatch loop calls
    /// `set_device_irqs(true)` once it begins steady-state dispatching, so
    /// every in-kernel task and kthread it runs executes with device
    /// interrupts *enabled*. A long in-kernel operation (a slow MMIO
    /// bring-up read, a busy driver poll) can therefore no longer mask
    /// interrupts for its whole span and starve the preemption one-shot,
    /// the buffered-serial transmit drain, or an interrupt-driven
    /// waiter — the cooperative dispatch loop the charter forbids. A device IRQ
    /// taken while an in-kernel task runs services its source and returns
    /// to the *same* task without rescheduling it (the kernel stays
    /// non-preemptible; only the timer-driven EL0 preemption point
    /// reschedules); its handler is lock-free and flags a deferred wake
    /// (see [`drain_pending_wakes`](crate::waitq::drain_pending_wakes)),
    /// so it can never deadlock against a lock the interrupted task holds.
    ///
    /// The loop masks again around the idle park (see
    /// [`Self::wait_for_interrupt`]) so the park/wake race is closed, and
    /// before [`Self::halt`].
    ///
    /// # Contract
    ///
    /// * Toggles only this CPU's PE-level IRQ mask; it never touches the
    ///   interrupt controller's per-line masks (those are the
    ///   mask-before-wake contract, `docs/src/security/irq.md`).
    /// * It must never panic.
    ///
    /// # Default
    ///
    /// A no-op: the `TestArch` mock and the `wasm32` port (which has no
    /// asynchronous PE interrupt mask to toggle) inherit zero work; a real
    /// bare-metal port overrides it with its `DAIF` / `sstatus.SIE` /
    /// `RFLAGS.IF` primitive.
    fn set_device_irqs(&self, enabled: bool) {
        let _ = enabled;
    }

    /// Top up any buffered console output to the device **without ever
    /// blocking** — the dispatch loop's per-iteration serial-drain hook.
    ///
    /// A port whose console transmit is buffered (the aarch64 PL011, whose
    /// flow-blocked Pi 4 UART would otherwise freeze the calling task for a
    /// whole line) copies producer bytes into an in-memory ring
    /// and drains it opportunistically. That ring must keep draining even
    /// when the dispatch loop never reaches its idle
    /// [`Self::wait_for_interrupt`] park — a perpetually-runnable in-kernel
    /// kthread (e.g. the polled USB-keyboard report pump, which yields every
    /// poll but never parks) keeps a task runnable forever, so an idle-only
    /// drain would stall the log the instant such a kthread exists, and the
    /// transmit-FIFO "has-room" interrupt cannot be relied on to self-sustain
    /// the drain on real silicon. The dispatch loop therefore calls this on
    /// **every** iteration — after each successful dispatch and again before
    /// it parks — so buffered output flows at the loop's rate regardless of
    /// idle and independent of the transmit interrupt (no
    /// service is starved by a busy in-kernel task).
    ///
    /// # Contract
    ///
    /// * It **must not** block or busy-wait on the device: push only what the transmitter accepts right now and return.
    /// * It must be safe to call with device IRQs either enabled or masked,
    ///   and must leave the CPU's interrupt-mask state unchanged.
    /// * It must never panic.
    ///
    /// # Default
    ///
    /// A no-op: the `TestArch` mock and ports whose console transmit is
    /// **synchronous** with no buffered ring (the riscv64 SBI console, the
    /// x86_64 COM1 sink) have nothing to top up and inherit zero work; the
    /// aarch64 port overrides it with its non-blocking `serial::pump_tx`.
    fn pump_console_tx(&self) {}
}

/// IRQ routing handed from the architecture port to the kernel core
/// during [`crate::Phase::Irq`].
///
/// `max_line` is the inclusive upper bound on user-visible IRQ lines;
/// `controller` is the `'static`-lifetime [`IrqController`] the trap
/// dispatcher invokes to honour the mask-before-wake ordering. The
/// reference is shared (`+ Sync`) because [`rustos_kernel_irq::IrqTable::fire`]
/// is called from interrupt context, possibly on multiple CPUs.
///
/// # Invariants
///
/// * `controller` must be safe to invoke from any CPU at any IRQ
///   level the architecture supports.
/// * The reference must outlive the running kernel — the arch port
///   typically backs it with a `Box::leak`'d allocation or a `static`.
#[derive(Copy, Clone)]
pub struct IrqRouting {
    /// Inclusive upper bound on `IrqTable::bind` line numbers.
    pub max_line: u32,
    /// `'static`-lifetime controller invoked by `IrqTable::fire`.
    pub controller: &'static (dyn IrqController + Send + Sync),
}

impl IrqRouting {
    /// The conservative fail-closed routing: `max_line = 0`, every
    /// `mask` returns [`rustos_kernel_irq::MaskError::Unsupported`].
    ///
    /// This is the [`KernelArch::irq_routing`] default; architecture
    /// ports with a programmable interrupt controller override the
    /// trait method and return their own routing.
    #[must_use]
    pub const fn unsupported() -> Self {
        Self {
            max_line: 0,
            controller: &UNSUPPORTED_CONTROLLER,
        }
    }
}

impl core::fmt::Debug for IrqRouting {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The controller is a trait object with no `Debug` super-trait
        // (adding one would expand the public surface); print the `max_line` and the controller's address
        // so the boot audit record carries an identity-stable
        // discriminator without forcing the supertrait change.
        f.debug_struct("IrqRouting")
            .field("max_line", &self.max_line)
            .field("controller_addr", &{
                // The controller is a `&'static dyn IrqController`, a
                // fat pointer (data + vtable). We only print the data
                // half because vtable identity is irrelevant for log
                // discrimination; binding the local pointer
                // explicitly avoids the `ptr_as_ptr` and
                // `incompatible_msrv` lints while keeping the cast
                // explicit at the source level.
                let p: *const dyn IrqController = self.controller;
                p.cast::<()>() as usize
            })
            .finish()
    }
}

/// Architecture-neutral kernel handover record.
///
/// Built by the Stage 3 arch crate from the platform's native boot
/// protocol and passed by value to [`crate::kernel_main`]. The
/// fields are intentionally typed (not raw integers) so the arch port
/// is forced through the same validators every other call site uses
/// (*validate every input*).
pub struct BootInfo<'a, A>
where
    A: KernelArch + 'static,
{
    /// Identifier of the boot processor — the CPU that is currently
    /// executing [`crate::kernel_main`].
    ///
    /// # SAFETY-INVARIANT
    ///
    /// Must equal `arch.current_cpu()` at the moment `kernel_main` is
    /// entered. `kernel_main` re-asserts this in a release-safe
    /// `debug_assert_eq!` to catch arch porting bugs that would
    /// otherwise route IPIs to the wrong CPU.
    pub boot_cpu: CpuId,

    /// Total number of logical CPUs the arch port intends to bring up.
    ///
    /// # SAFETY-INVARIANT
    ///
    /// `cpu_count >= 1` and `boot_cpu < cpu_count`. Both are
    /// re-asserted by [`Self::validate`] / [`crate::kernel_main`].
    pub cpu_count: u32,

    /// Kernel command line as parsed by the bootloader.
    ///
    /// May be empty. Stored as a borrowed `&str` so the early boot path
    /// never allocates; the arch port owns the backing storage for the
    /// lifetime of `kernel_main`'s call.
    pub command_line: &'a str,

    /// Typed physical-memory map produced by the bootloader.
    ///
    /// # SAFETY-INVARIANT
    ///
    /// Every [`rustos_kernel_mem::MemoryRegion`] of kind
    /// [`rustos_kernel_mem::RegionKind::Usable`] is genuinely free RAM —
    /// the bootloader has flushed and invalidated any caches, and no
    /// firmware service still owns the range. Violations corrupt the
    /// frame allocator immediately; the arch port is the only place
    /// that can vouch for this and is reviewed accordingly.
    pub memory_map: BootMemoryMap,

    /// Static scheduler configuration.
    ///
    /// # SAFETY-INVARIANT
    ///
    /// `scheduler_config.cpus == cpu_count`. Re-asserted by
    /// [`Self::validate`]; the scheduler would otherwise mis-size its
    /// per-CPU array.
    pub scheduler_config: SchedulerConfig,

    /// Architecture port instance.
    ///
    /// Stored inside an `Arc` because the scheduler (and, downstream,
    /// the syscall dispatcher landed in Stage 2.7) hold a clone for
    /// the lifetime of the running kernel.
    pub arch: Arc<A>,

    /// Sink that receives every kernel log record (everything routed
    /// through `lib/log`'s [`rustos_log::log`]).
    ///
    /// `'static` because the sink lives for the lifetime of the
    /// running kernel; the arch port typically constructs it from a
    /// static UART/framebuffer/ring-buffer driver.
    pub log_sink: &'static (dyn Sink + Sync),

    /// Sink that receives security-relevant audit records emitted by
    /// `kernel/sec`, `kernel/ipc`, and (Stage 2.7) `kernel/syscall`.
    ///
    /// Production ports route this to a tamper-evident store separate
    /// from the diagnostic log; host tests use the same `TestSink` for
    /// both.
    pub audit_sink: &'static (dyn Sink + Sync),

    /// Initial global log-level filter to install before the first
    /// phase event is emitted.
    pub log_level: Level,

    /// Bin-crate-owned slot through which [`crate::kernel_main`]
    /// publishes the production syscall [`crate::DispatchHook`]
    /// during the `Phase::Syscall` init step.
    ///
    /// Stage 2.7 follow-up (f4). The slot is a `'static` reference
    /// because the bin crate owns the underlying
    /// [`DispatchCallbackSlot`] for the lifetime of the running
    /// kernel (typically as a `static` in the binary crate, anchored
    /// at compile time — no global *mutable* static; the
    /// [`DispatchCallbackSlot`]'s internal `OnceCell` is set-once,
    /// see `kernel/sync::once`).
    ///
    /// The arch-port's `set_dispatch_callback` is **still** invoked
    /// before `syscall` is enabled — this field is the *kernel-side*
    /// publication point only, not the trampoline. The two channels
    /// are documented in `docs/src/architecture/kernel.md`'s
    /// "Syscall registration phase" section.
    pub dispatcher_callback_slot: &'static DispatchCallbackSlot,

    /// The installed system console list the `stream_write` / `stream_read`
    /// syscalls (`abi-v1` numbers 11 / 13) resolve a descriptor's console
    /// index against (`plans/PI.md` P6 / P11): index 0 the primary console (the detected display when
    /// present, else the first discovered UART), each further entry an
    /// independent console with its own session context (the UART beside
    /// an active video console).
    ///
    /// Defaults to the empty [`NO_CONSOLES`], so every console-backed
    /// stream access fails closed with
    /// [`rustos_abi::Errno::NotImplemented`]: an arch
    /// port that has not discovered a console leaves this default and the
    /// streams announce an inert interface rather than touching a device
    /// that does not exist. A port installs its discovered list through
    /// [`Self::with_consoles`]. The raw devices are installed here; the
    /// kernel-core init pipeline wraps each read half in
    /// [`crate::console::BlockingConsoleRead`] before handing the list to
    /// the syscall layer (the backing owns blocking).
    /// Held as a `'static` borrow because the installed consoles live for
    /// the lifetime of the running kernel, exactly like the log/audit
    /// sinks.
    pub consoles: &'static [ConsoleDevice],

    /// Architecture-specific seam that spawns PID 1 (`init`) into user
    /// mode once boot completes (`plans/PI.md` P6c-3).
    ///
    /// Defaults to `None`: a port that has no user-mode bring-up wired
    /// yet leaves it unset and [`crate::kernel_main`] parks the boot CPU
    /// after [`crate::AuditEvent::BootCompleted`], exactly as before. A
    /// port that can reach user mode installs its [`InitSpawn`] through
    /// [`Self::with_init`]; `kernel_main` then invokes it after
    /// `BootCompleted` and the implementation diverges into the spawned
    /// program (the arch-specific page-table /
    /// `EnterUser` types live in the port, not the core). Held as a
    /// `'static` borrow because the seam lives for the lifetime of the
    /// running kernel, like the console and the sinks.
    pub init: Option<&'static dyn InitSpawn>,

    /// Embedded-program registry the `spawn` syscall resolves a program
    /// path against (`plans/SPAWN.md` SP3).
    ///
    /// Defaults to [`EMPTY_PROGRAM_REGISTRY`]: a `spawn` of any path then
    /// fails closed with [`rustos_abi::Errno::NotFound`] until the arch
    /// port installs a populated registry through [`Self::with_spawn`]. The
    /// program bytes are `'static` (the host-only `elf2rxe` build glue bakes
    /// them into the kernel image), so the registry lives
    /// for the running kernel's lifetime, exactly like the console device.
    pub programs: &'static ProgramRegistry,

    /// Architecture-specific producer the `spawn` syscall drives to build a
    /// child's hardware-isolated address space and admit it as a runnable
    /// process (`plans/SPAWN.md` SP3).
    ///
    /// Defaults to [`NULL_PROCESS_SPAWN`], which fails closed with
    /// [`rustos_abi::Errno::NotImplemented`]: a port that
    /// has no runtime-spawn producer wired leaves this default and `spawn`
    /// announces an inert subsystem rather than half-building a task. A port
    /// that can build a child address space installs its producer through
    /// [`Self::with_spawn`]; spawning is *not* a privileged bypass — the
    /// child receives only its manifest∩user-grant authority. Held as a `'static` borrow, like the console device.
    pub spawn_service: &'static (dyn ProcessSpawn + 'static),

    /// The on-disk application store the `spawn` syscall resolves a
    /// non-embedded `…/<Name>.app/Run` path against (`plans/APPS.md`
    /// deliverable 8): the build's embedded app trust anchor plus the
    /// readiness latch the boot path resolves when the `/System` mount
    /// reaches a terminal state.
    ///
    /// Defaults to `None`: a port with no storage floor leaves it unset and
    /// a store-bundle spawn fails closed with
    /// [`rustos_abi::Errno::NotFound`], parking nothing. A port whose boot
    /// path will publish (or explicitly give up on) the `/System` mount
    /// installs its store through [`Self::with_app_store`].
    pub app_store: Option<&'static crate::appspawn::AppStore>,

    /// The kernel seat registry the keyboard syscalls (`key_inject` /
    /// `display_acquire` / `display_release` / `keyboard_read`) drive
    /// (`plans/DISPLAY.md` D2, `plans/PI.md` P11 — input follows
    /// the surface owner).
    ///
    /// Defaults to [`NULL_SEAT_REGISTRY`], whose text sink is the fail-closed
    /// [`crate::console::NULL_CONSOLE_INPUT`]: an arch port that has wired no
    /// registry leaves this default and a `key_inject` on the unowned seat
    /// fails closed rather than leaking a key edge to a device. A port
    /// installs its registry — its text sink pointed at the
    /// console that owns the directly attached keyboard — through
    /// [`Self::with_seat_registry`]. Held as a `'static` borrow because the
    /// registry lives for the lifetime of the running kernel, exactly like the
    /// console list.
    pub seat_registry: &'static SeatRegistry,

    /// The users-database holder the `users_db_read` syscall (no. 19,
    /// `CAP_USERS_READ`) serves to the login session (
    /// `plans/PI.md` P11).
    ///
    /// Defaults to [`NULL_USERS_DB`], whose [`UsersDbSource::text`] fails
    /// closed with [`rustos_abi::Errno::NotImplemented`]: a boot path that
    /// has not mounted the root volume leaves this default and
    /// `users_db_read` announces an inert interface — login then runs its
    /// deny-all authenticator and refuses every attempt rather than
    /// inventing accounts. A boot path that
    /// mounts the root volume runs the audited
    /// [`crate::load_users_db_source`] read and installs the resulting
    /// `Box::leak`'d [`crate::HeldUsersDbSource`] through
    /// [`Self::with_users_db`]; `kernel_main` then threads it into the
    /// production dispatch hook. Held as a `'static` borrow because the
    /// holder lives for the lifetime of the running kernel, exactly like
    /// the console list (the install is a one-shot
    /// move, not a global mutable static).
    pub users_db: &'static (dyn UsersDbSource + 'static),

    /// The account-administration engine the `users_admin` syscall
    /// dispatches into (`plans/CAPABILITY_USE.md` CU4).
    ///
    /// Defaults to [`crate::useradmin::NULL_USERS_ADMIN`], which fails
    /// closed with [`rustos_abi::Errno::NotImplemented`] — a boot path
    /// with no unlocked root leaves the default and every `users_admin`
    /// call is refused. A boot path that unlocked the root hands the
    /// same `&'static LateUsersAdmin` cell its unlock step later
    /// installs the built [`crate::useradmin::UserAdminEngine`] into
    /// through [`Self::with_users_admin`]. Held `'static` like the
    /// users database it administers.
    pub users_admin: &'static (dyn UsersAdmin + 'static),

    /// The discovered hardware-tree store the `hw_tree_read` (no. 29) /
    /// `hw_tree_wait` (no. 30) syscalls serve (Design D).
    ///
    /// Defaults to [`NULL_HW_TREE`], whose reads fail closed with
    /// [`rustos_abi::Errno::NotImplemented`]: a boot path that seeds no
    /// inventory leaves this default and both syscalls announce an inert
    /// interface. A boot path that seeds the discovered
    /// tree installs its store through [`Self::with_hw_tree`];
    /// `kernel_main` then threads it into the production dispatch hook.
    /// Held as a `'static` borrow because the store lives for the lifetime
    /// of the running kernel, exactly like the users database.
    pub hw_tree: &'static (dyn HwTreeSource + 'static),

    /// The kernel filesystem service the `fs_*` syscalls route through
    /// (`PREREQUISITES.md` P-A).
    ///
    /// Defaults to [`NULL_FILESYSTEM`], whose every operation fails closed
    /// with [`rustos_abi::Errno::NotImplemented`]: a boot path that has not
    /// mounted a volume leaves this default and every `fs_*` syscall announces
    /// an inert interface rather than fabricating a handle or a read. A boot
    /// path that owns a mounted volume installs the disk-backed service
    /// through [`Self::with_filesystem`]; `kernel_main` then threads it into
    /// the production dispatch hook. Held as a `'static` borrow because the
    /// mounted filesystem lives for the lifetime of the running kernel,
    /// exactly like the users database.
    pub filesystem: &'static (dyn FilesystemService + 'static),

    /// The volume forest the `id::` path resolver reads
    /// (`plans/DEVICES.md` D3a).
    ///
    /// Defaults to [`NULL_VOLUME_FOREST`], into which nothing is ever
    /// published, so every `id::<volume-id>/…` resolution fails closed with
    /// [`rustos_abi::Errno::NotFound`]. A boot path that mounts volumes
    /// installs its own forest through [`Self::with_volumes`] and publishes
    /// each mounted volume's stable identity into it; `kernel_main` then
    /// threads it into the production dispatch hook. Held as a `'static`
    /// borrow because the forest lives for the lifetime of the running
    /// kernel, exactly like the filesystem service.
    pub volumes: &'static VolumeForest,

    /// The runtime volume attach/detach service the `volume_attach` /
    /// `volume_detach` syscalls delegate to (`plans/DEVICES.md` D3b).
    ///
    /// Defaults to [`NULL_VOLUME_SERVICE`], so every attach/detach fails
    /// closed with [`rustos_abi::Errno::NotImplemented`]. A boot path
    /// that can host runtime volumes installs its service through
    /// [`Self::with_volume_service`]; `kernel_main` then threads it into
    /// the production dispatch hook. Held as a `'static` borrow, exactly
    /// like the filesystem service.
    pub volume_service: &'static (dyn VolumeService + 'static),

    /// The boot path's authoritative identity cell: the `spawn` handler
    /// resolves a spawn-as-user switch against it, and the filesystem
    /// service resolves caller groups against the *same* cell — one
    /// authoritative table, no second copy.
    ///
    /// During the `sec` phase [`crate::kernel_main`] builds the
    /// compiled-in system identity (the OS-owned accounts and groups,
    /// kernel policy — `rustos_users::system_accounts`) and installs it
    /// into this cell, so the system and service accounts resolve from
    /// first boot on every architecture, before any volume exists. The
    /// encrypted-root unlock later replaces the held table with the merge
    /// of that same compiled half and the on-disk human accounts.
    ///
    /// Defaults to `None`: a boot path (or host harness) that wires no
    /// identity cell gets no install, `kernel_main` threads the inert
    /// [`crate::syscalls::NULL_IDENTITY`] into the dispatch hook, and
    /// every credential resolution fails closed with
    /// [`rustos_abi::Errno::NotImplemented`]. A production port installs
    /// its cell through [`Self::with_spawn_identity`]. The default
    /// `spawn` (inherit) never consults it. Held `'static` because the
    /// cell lives for the running kernel's lifetime.
    pub spawn_identity: Option<&'static LateIdentity>,

    /// Committed size, in bytes, of the kernel heap region — reported as
    /// [`rustos_abi::sysinfo::KernelMemoryStats::kernel_heap_bytes`] by the
    /// System Information introspection source (`PREREQUISITES.md` P-C).
    ///
    /// `kernel/core` does not own the `#[global_allocator]` (the binding
    /// kernel does, over `rustos_kalloc`), so the boot path threads its
    /// committed heap size here. Defaults to `0` — a boot path that does not
    /// set it reports "no kernel heap accounted" rather than a fabricated
    /// figure.
    pub kernel_heap_bytes: u64,

    /// Installed physical memory, in bytes, as the boot path's platform
    /// memory source reports it (the firmware map / device-tree memory
    /// node) **before** any kernel carve-outs — reported through the
    /// ungated `boot_facts_get` syscall as the machine's installed-RAM
    /// figure.
    ///
    /// The post-carve [`Self::memory_map`] cannot recover this figure (a
    /// carved kernel-image range is dropped from the map entirely), so the
    /// boot path threads the pre-carve total here. Defaults to `0` — a boot
    /// path that does not set it leaves the boot facts uninstalled and
    /// `boot_facts_get` fails closed rather than reporting a fabricated
    /// figure.
    pub installed_memory_bytes: u64,

    // Holds the lifetime parameter (covers `command_line`). The PhantomData
    // is invariant in `'a` so callers cannot accidentally extend the
    // borrow.
    _marker: core::marker::PhantomData<&'a ()>,
}

impl<'a, A> BootInfo<'a, A>
where
    A: KernelArch + 'static,
{
    /// Construct a [`BootInfo`].
    ///
    /// The arguments mirror the struct fields exactly; this constructor
    /// exists so that adding a new field later is a single edit per
    /// arch port instead of a search-and-replace across struct literal
    /// expressions (no interface creep manifests as
    /// no naked struct literals).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        boot_cpu: CpuId,
        cpu_count: u32,
        command_line: &'a str,
        memory_map: BootMemoryMap,
        scheduler_config: SchedulerConfig,
        arch: Arc<A>,
        log_sink: &'static (dyn Sink + Sync),
        audit_sink: &'static (dyn Sink + Sync),
        log_level: Level,
        dispatcher_callback_slot: &'static DispatchCallbackSlot,
    ) -> Self {
        Self {
            boot_cpu,
            cpu_count,
            command_line,
            memory_map,
            scheduler_config,
            arch,
            log_sink,
            audit_sink,
            log_level,
            dispatcher_callback_slot,
            // Fail closed until the arch port installs its discovered
            // console list through `with_consoles`.
            consoles: &NO_CONSOLES,
            // No user-mode bring-up until the arch port installs an
            // `InitSpawn` through `with_init`; `kernel_main` halts after
            // `BootCompleted` until then (`plans/PI.md` P6c-3).
            init: None,
            // Spawn subsystem unwired until the arch port threads a populated
            // registry + producer through `with_spawn` (`plans/SPAWN.md`
            // SP3): `spawn` fails closed (`NotFound` / `NotImplemented`).
            programs: &EMPTY_PROGRAM_REGISTRY,
            spawn_service: &NULL_PROCESS_SPAWN,
            // No on-disk application store until a boot path with a storage
            // floor installs one: a store-bundle spawn fails closed.
            app_store: None,
            // Seat registry unwired until the arch port installs the
            // real one through `with_seat_registry` (`plans/DISPLAY.md` D2):
            // `key_inject` / `keyboard_read` fail closed through
            // `NULL_SEAT_REGISTRY`.
            seat_registry: &NULL_SEAT_REGISTRY,
            // Users database unwired until a boot path mounts the root
            // volume and installs the loaded holder through
            // `with_users_db` (`plans/PI.md` P11): `users_db_read` fails
            // closed through `NULL_USERS_DB`.
            users_db: &NULL_USERS_DB,
            users_admin: &NULL_USERS_ADMIN,
            // Hardware-tree store unwired until a boot path seeds the
            // discovered inventory and installs its store through
            // `with_hw_tree` (Design D): `hw_tree_read` / `hw_tree_wait`
            // fail closed through `NULL_HW_TREE`.
            hw_tree: &NULL_HW_TREE,
            // Filesystem service unwired until a boot path mounts a volume
            // and installs the disk-backed service through `with_filesystem`
            // (`PREREQUISITES.md` P-A): every `fs_*` syscall fails closed
            // through `NULL_FILESYSTEM`.
            filesystem: &NULL_FILESYSTEM,
            // Volume forest unwired until a boot path mounts volumes and
            // installs its forest through `with_volumes`
            // (`plans/DEVICES.md` D3a): every `id::` resolution fails
            // closed through `NULL_VOLUME_FOREST`.
            volumes: &NULL_VOLUME_FOREST,
            // Volume attach/detach service unwired until a boot path that
            // can host runtime volumes installs it through
            // `with_volume_service` (`plans/DEVICES.md` D3b): every
            // attach/detach fails closed through `NULL_VOLUME_SERVICE`.
            volume_service: &NULL_VOLUME_SERVICE,
            // No identity cell until the port installs one through
            // `with_spawn_identity` (`PREREQUISITES.md` P-C): the sec phase
            // then has nowhere to publish the compiled-in system identity
            // and every credential resolution fails closed through the
            // inert `NULL_IDENTITY`.
            spawn_identity: None,
            // Kernel heap size unaccounted until the binding kernel threads
            // its committed `rustos_kalloc::HEAP_BYTES` through
            // `with_kernel_heap_bytes`; reported as `0` until then.
            kernel_heap_bytes: 0,
            // Installed memory unknown until the boot path threads its
            // pre-carve platform total through `with_installed_memory`;
            // the boot facts stay uninstalled (fail closed) until then.
            installed_memory_bytes: 0,
            _marker: core::marker::PhantomData,
        }
    }

    /// Record the committed size, in bytes, of the kernel heap region,
    /// consuming and returning `self` (`PREREQUISITES.md` P-C).
    ///
    /// The binding kernel owns the `#[global_allocator]`, so it passes its
    /// `rustos_kalloc::HEAP_BYTES` here; `kernel_main` threads it into the
    /// introspection source so the `KernelMemory` domain reports a truthful
    /// committed-heap figure. A boot path that never calls it reports `0`.
    #[must_use]
    pub const fn with_kernel_heap_bytes(mut self, bytes: u64) -> Self {
        self.kernel_heap_bytes = bytes;
        self
    }

    /// Record the installed physical memory, in bytes, as the platform's
    /// boot memory source reports it before any kernel carve-outs,
    /// consuming and returning `self`.
    ///
    /// `kernel_main` mints the [`rustos_abi::BootFacts`] record from this
    /// figure (with the arch identity and CPU count) and installs it into
    /// the syscall layer, so the ungated `boot_facts_get` reports the
    /// machine's true installed RAM. A boot path that never calls it — or
    /// passes `0` — leaves the facts uninstalled and the syscall failing
    /// closed rather than reporting a fabricated figure.
    #[must_use]
    pub const fn with_installed_memory(mut self, bytes: u64) -> Self {
        self.installed_memory_bytes = bytes;
        self
    }

    /// Install the discovered system console list the stream syscalls
    /// resolve descriptors against, consuming and returning `self`.
    ///
    /// Called by an arch port's boot pipeline after it has selected the
    /// console devices from the normalised hardware tree (`plans/PI.md`
    /// P6 / P11): index 0 the primary console (the
    /// detected display when present, else the first discovered UART),
    /// each further entry an independent console with its own session
    /// context. Until this is called the handover holds the empty
    /// [`NO_CONSOLES`] and every console-backed stream access fails
    /// closed with [`rustos_abi::Errno::NotImplemented`]. The list must
    /// be `'static`: the boot path leaks it alongside the kernel state,
    /// which lives for the lifetime of the running kernel (the install is a one-shot move, not a global mutable
    /// static).
    #[must_use]
    pub fn with_consoles(mut self, consoles: &'static [ConsoleDevice]) -> Self {
        self.consoles = consoles;
        self
    }

    /// Install the [`InitSpawn`] seam [`crate::kernel_main`] invokes to
    /// spawn PID 1 into user mode after boot completes, consuming and
    /// returning `self` (`plans/PI.md` P6c-3).
    ///
    /// Called by an arch port's boot pipeline (or the kernel binary that
    /// wires it) once it can build a user address space and reach user
    /// mode. Until this is called the handover holds `None` and
    /// `kernel_main` parks the boot CPU after
    /// [`crate::AuditEvent::BootCompleted`]. The seam must be `'static`:
    /// the boot path leaks it alongside the kernel state, which lives for
    /// the lifetime of the running kernel (the install
    /// is a one-shot move, not a global mutable static).
    #[must_use]
    pub fn with_init(mut self, init: &'static dyn InitSpawn) -> Self {
        self.init = Some(init);
        self
    }

    /// Install the embedded-program registry and the architecture spawn
    /// producer the `spawn` syscall drives, consuming and returning `self`
    /// (`plans/SPAWN.md` SP3).
    ///
    /// Called by an arch port's boot pipeline once it can build a child
    /// address space and has embedded programs to launch. Until this is
    /// called the handover holds [`EMPTY_PROGRAM_REGISTRY`] and
    /// [`NULL_PROCESS_SPAWN`], so `spawn` fails closed
    /// ([`rustos_abi::Errno::NotFound`] / [`rustos_abi::Errno::NotImplemented`]).
    /// Both must be `'static`: the program bytes and the producer live for
    /// the lifetime of the running kernel, exactly like the console device
    /// (the install is a one-shot move, not a global
    /// mutable static).
    #[must_use]
    pub fn with_spawn(
        mut self,
        programs: &'static ProgramRegistry,
        spawn_service: &'static (dyn ProcessSpawn + 'static),
    ) -> Self {
        self.programs = programs;
        self.spawn_service = spawn_service;
        self
    }

    /// Install the on-disk application store the `spawn` syscall resolves
    /// non-embedded `…/<Name>.app/Run` paths against, consuming and
    /// returning `self` (`plans/APPS.md` deliverable 8).
    ///
    /// Called by a boot pipeline whose storage floor will publish (or
    /// explicitly give up on) the `/System` mount and resolve the store's
    /// readiness latch on every outcome. Until this is called a
    /// store-bundle spawn fails closed ([`rustos_abi::Errno::NotFound`]),
    /// parking nothing. The store must be `'static`, exactly like the
    /// program registry.
    #[must_use]
    pub fn with_app_store(mut self, app_store: &'static crate::appspawn::AppStore) -> Self {
        self.app_store = Some(app_store);
        self
    }

    /// Install the kernel seat registry the keyboard syscalls drive,
    /// consuming and returning `self` (`plans/DISPLAY.md` D2).
    ///
    /// Called by an arch port's boot pipeline after it has built the registry
    /// with its text sink pointed at the console that owns the directly
    /// attached keyboard (on the Pi, the video console's input queue). Until
    /// this is called the handover holds [`NULL_SEAT_REGISTRY`] and a
    /// `key_inject` on the default unowned seat fails closed. The registry must be `'static`: the boot path leaks it alongside
    /// the kernel state, which lives for the lifetime of the running kernel
    /// (the install is a one-shot move, not a global
    /// mutable static).
    #[must_use]
    pub fn with_seat_registry(mut self, seat_registry: &'static SeatRegistry) -> Self {
        self.seat_registry = seat_registry;
        self
    }

    /// Install the loaded users-database holder the `users_db_read`
    /// syscall serves, consuming and returning `self` (`plans/PI.md`
    /// P11).
    ///
    /// Called by a boot path that mounted the root volume and ran the
    /// audited [`crate::load_users_db_source`] read, handing the
    /// `Box::leak`'d [`crate::HeldUsersDbSource`] here. Until this is
    /// called the handover holds [`NULL_USERS_DB`] and `users_db_read`
    /// fails closed, so login refuses every attempt rather than inventing
    /// accounts. The holder must be `'static`: the
    /// boot path leaks it alongside the kernel state, which lives for the
    /// lifetime of the running kernel (the install is a
    /// one-shot move, not a global mutable static).
    #[must_use]
    pub fn with_users_db(mut self, users_db: &'static (dyn UsersDbSource + 'static)) -> Self {
        self.users_db = users_db;
        self
    }

    /// Install the account-administration facility the `users_admin`
    /// syscall dispatches into, consuming and returning `self`
    /// (`plans/CAPABILITY_USE.md` CU4).
    ///
    /// Called by the boot path that unlocks the encrypted root, handing
    /// the `&'static LateUsersAdmin` cell its unlock step installs the
    /// built engine into. Until then the handover holds
    /// [`crate::useradmin::NULL_USERS_ADMIN`] and every `users_admin`
    /// call fails closed with [`rustos_abi::Errno::NotImplemented`].
    #[must_use]
    pub fn with_users_admin(mut self, users_admin: &'static (dyn UsersAdmin + 'static)) -> Self {
        self.users_admin = users_admin;
        self
    }

    /// Install the discovered hardware-tree store the `hw_tree_read` /
    /// `hw_tree_wait` syscalls serve, consuming and returning `self`.
    ///
    /// Called by a boot path after it seeds the discovered inventory,
    /// handing the store (typically a `&'static` wrapper over the binding
    /// kernel's authoritative `HwTreeStore`) here. Until this is called the
    /// handover holds [`NULL_HW_TREE`] and both syscalls fail closed. The store must be `'static`: it lives for the
    /// lifetime of the running kernel, exactly like the users database.
    #[must_use]
    pub fn with_hw_tree(mut self, hw_tree: &'static (dyn HwTreeSource + 'static)) -> Self {
        self.hw_tree = hw_tree;
        self
    }

    /// Install the disk-backed filesystem service the `fs_*` syscalls route
    /// through, consuming and returning `self` (`PREREQUISITES.md` P-A).
    ///
    /// Called by a boot path that owns a mounted volume, handing the
    /// `Box::leak`'d production service here. Until this is called the
    /// handover holds [`NULL_FILESYSTEM`] and every `fs_*` syscall fails
    /// closed with [`rustos_abi::Errno::NotImplemented`], so userland sees an
    /// inert filesystem rather than a fabricated handle. The service must be
    /// `'static`: it lives for the lifetime of the running kernel, exactly
    /// like the users database.
    #[must_use]
    pub fn with_filesystem(
        mut self,
        filesystem: &'static (dyn FilesystemService + 'static),
    ) -> Self {
        self.filesystem = filesystem;
        self
    }

    /// Install the volume forest the `id::` path resolver reads, consuming
    /// and returning `self` (`plans/DEVICES.md` D3a).
    ///
    /// Called by a boot path that mounts volumes, handing the `'static`
    /// forest it publishes each mounted volume's stable identity into.
    /// Until this is called the handover holds [`NULL_VOLUME_FOREST`] and
    /// every `id::<volume-id>/…` path fails closed with
    /// [`rustos_abi::Errno::NotFound`].
    #[must_use]
    pub const fn with_volumes(mut self, volumes: &'static VolumeForest) -> Self {
        self.volumes = volumes;
        self
    }

    /// Install the runtime volume attach/detach service the
    /// `volume_attach` / `volume_detach` syscalls delegate to, consuming
    /// and returning `self` (`plans/DEVICES.md` D3b).
    ///
    /// Called by a boot path that can host runtime volumes. Until this is
    /// called the handover holds [`NULL_VOLUME_SERVICE`] and every
    /// attach/detach fails closed with
    /// [`rustos_abi::Errno::NotImplemented`].
    #[must_use]
    pub const fn with_volume_service(
        mut self,
        volume_service: &'static (dyn VolumeService + 'static),
    ) -> Self {
        self.volume_service = volume_service;
        self
    }

    /// Install the boot path's identity cell — the one the `sec` phase
    /// publishes the compiled-in system identity into, the `spawn` handler
    /// resolves a spawn-as-user switch against, and the filesystem service
    /// resolves caller groups against — consuming and returning `self`
    /// (`PREREQUISITES.md` P-C).
    ///
    /// Every production port hands the **same** `&'static LateIdentity` the
    /// encrypted-root unlock later replaces with the merged system∪human
    /// table (one authoritative table, no second copy). Until this is
    /// called the handover carries `None`: nothing is installed, the
    /// dispatch hook falls to the inert [`crate::syscalls::NULL_IDENTITY`],
    /// and a spawn-as-user switch fails closed with
    /// [`rustos_abi::Errno::NotImplemented`]; the default `spawn` (inherit)
    /// never consults it. The cell must be `'static`: it lives for the
    /// lifetime of the running kernel, exactly like the filesystem service.
    #[must_use]
    pub const fn with_spawn_identity(mut self, spawn_identity: &'static LateIdentity) -> Self {
        self.spawn_identity = Some(spawn_identity);
        self
    }

    /// Verify the SAFETY-INVARIANTs documented on each field.
    ///
    /// Called once at the top of [`crate::kernel_main`] before any
    /// subsystem init runs. Returns a [`BootInfoError`] if any
    /// invariant is violated; the caller logs it and halts.
    ///
    /// The intent is *release-safe* validation: every check is a cheap
    /// integer comparison, so we do not gate them on `debug_assertions`
    /// (fail closed).
    ///
    /// # Errors
    ///
    /// Returns a [`BootInfoError`] naming the violated invariant.
    pub fn validate(&self) -> Result<(), BootInfoError> {
        if self.cpu_count == 0 {
            return Err(BootInfoError::ZeroCpus);
        }
        if self.boot_cpu >= self.cpu_count {
            return Err(BootInfoError::BootCpuOutOfRange);
        }
        if self.scheduler_config.cpus != self.cpu_count {
            return Err(BootInfoError::SchedulerCpuMismatch);
        }
        if self.command_line.len() > MAX_COMMAND_LINE_BYTES {
            return Err(BootInfoError::CommandLineTooLong);
        }
        // The remaining invariants (memory-map coherence, sink
        // liveness) are upheld by the dedicated subsystem constructors
        // — `FrameAllocator::new` re-validates the memory map, and
        // sinks are `&'static` references, so a dangling sink is a
        // type-system error rather than a runtime one.
        Ok(())
    }
}

/// Hard cap on the kernel command line length.
///
/// Chosen at one page (`4 KiB`) minus a small headroom; longer command
/// lines indicate either a misconfigured bootloader or an attempt to
/// flood the early-boot log. Either way the kernel refuses to boot
/// rather than silently truncating (fail closed).
pub const MAX_COMMAND_LINE_BYTES: usize = 4096 - 16;

/// Reason [`BootInfo::validate`] rejected a handover record.
///
/// Each variant corresponds 1:1 to a documented SAFETY-INVARIANT on
/// [`BootInfo`]; new variants ship alongside new invariants.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum BootInfoError {
    /// `cpu_count == 0`.
    ZeroCpus,
    /// `boot_cpu >= cpu_count`.
    BootCpuOutOfRange,
    /// `scheduler_config.cpus != cpu_count`.
    SchedulerCpuMismatch,
    /// `command_line.len() > MAX_COMMAND_LINE_BYTES`.
    CommandLineTooLong,
}

impl BootInfoError {
    /// Short, fixed name suitable for inclusion in a log field.
    ///
    /// `lib/log` events do not allocate, so the panic and init-failure
    /// paths borrow these literals directly.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ZeroCpus => "zero_cpus",
            Self::BootCpuOutOfRange => "boot_cpu_out_of_range",
            Self::SchedulerCpuMismatch => "scheduler_cpu_mismatch",
            Self::CommandLineTooLong => "command_line_too_long",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::SchedulerConfig;
    use crate::test_arch::TestArch;
    use alloc::sync::Arc;
    use rustos_log::Level;

    fn empty_sink() -> &'static crate::test_sink::TestSink {
        // A `Box::leak`'d sink is intentional in tests — the sink
        // outlives every test, mirroring the `&'static` invariant the
        // production arch port upholds. `Box::leak` is permitted in
        // tests by.
        alloc::boxed::Box::leak(alloc::boxed::Box::new(crate::test_sink::TestSink::new()))
    }

    fn leak_dispatch_slot() -> &'static DispatchCallbackSlot {
        // `Box::leak` mirrors the bin-crate convention: the slot
        // outlives every test, matching the `&'static` invariant the
        // production binary upholds with a `static`.
        // permits `Box::leak` in tests.
        alloc::boxed::Box::leak(alloc::boxed::Box::new(DispatchCallbackSlot::new()))
    }

    fn fresh_boot_info() -> BootInfo<'static, TestArch> {
        let arch = Arc::new(TestArch::with_cpus(1));
        BootInfo::new(
            0,
            1,
            "",
            BootMemoryMap::new(),
            SchedulerConfig::defaults_for(1),
            arch,
            empty_sink(),
            empty_sink(),
            Level::Info,
            leak_dispatch_slot(),
        )
    }

    #[test]
    fn validate_accepts_well_formed_handover() {
        assert_eq!(fresh_boot_info().validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_zero_cpus() {
        let mut b = fresh_boot_info();
        b.cpu_count = 0;
        b.scheduler_config = SchedulerConfig::defaults_for(0);
        assert_eq!(b.validate(), Err(BootInfoError::ZeroCpus));
    }

    #[test]
    fn validate_rejects_boot_cpu_out_of_range() {
        let mut b = fresh_boot_info();
        b.boot_cpu = 5;
        assert_eq!(b.validate(), Err(BootInfoError::BootCpuOutOfRange));
    }

    #[test]
    fn validate_rejects_scheduler_cpu_mismatch() {
        let mut b = fresh_boot_info();
        b.scheduler_config = SchedulerConfig::defaults_for(4);
        assert_eq!(b.validate(), Err(BootInfoError::SchedulerCpuMismatch));
    }

    #[test]
    fn validate_rejects_oversize_command_line() {
        // Use a static, leaked allocation for the oversize command line so
        // the borrow checker is satisfied without unsafe.
        let buf: &'static str = alloc::boxed::Box::leak(
            alloc::string::String::from_utf8(alloc::vec![b'x'; MAX_COMMAND_LINE_BYTES + 1])
                .expect("ascii")
                .into_boxed_str(),
        );
        let mut b = fresh_boot_info();
        b.command_line = buf;
        assert_eq!(b.validate(), Err(BootInfoError::CommandLineTooLong));
    }

    #[test]
    fn users_db_defaults_to_fail_closed_and_with_users_db_installs_a_holder() {
        use rustos_abi::Errno;

        // A test users-db source serving fixed bytes, leaked to `'static`
        // exactly as a production boot leaks its `HeldUsersDbSource`.
        struct FixedUsersDb;
        impl UsersDbSource for FixedUsersDb {
            fn text(&self) -> Result<crate::users::UsersDbText, Errno> {
                Ok(crate::users::UsersDbText::new(
                    b"rustos-users-v1\n".to_vec(),
                ))
            }
        }

        // Default handover: `users_db_read` is inert.
        let b = fresh_boot_info();
        assert_eq!(b.users_db.text(), Err(Errno::NotImplemented));

        // After install the handover serves the holder's bytes.
        let held: &'static FixedUsersDb =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(FixedUsersDb));
        let b = fresh_boot_info().with_users_db(held);
        assert_eq!(
            b.users_db.text().expect("served text"),
            &b"rustos-users-v1\n"[..]
        );
    }

    #[test]
    fn bootinfo_error_strings_are_stable() {
        assert_eq!(BootInfoError::ZeroCpus.as_str(), "zero_cpus");
        assert_eq!(
            BootInfoError::BootCpuOutOfRange.as_str(),
            "boot_cpu_out_of_range"
        );
        assert_eq!(
            BootInfoError::SchedulerCpuMismatch.as_str(),
            "scheduler_cpu_mismatch"
        );
        assert_eq!(
            BootInfoError::CommandLineTooLong.as_str(),
            "command_line_too_long"
        );
    }
}
