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

use rustos_arch_api::ContextSwitch;

use crate::sched::{CpuId, SchedulerArch, SchedulerConfig};
use rustos_kernel_irq::{IrqController, IrqTable, UNSUPPORTED_CONTROLLER};
use rustos_kernel_mem::BootMemoryMap;
use rustos_kernel_sec::IdentityTableBuilder;
use rustos_log::{Level, Sink};

use crate::console::{ConsoleDevice, NO_CONSOLES};
use crate::dispatch_slot::DispatchCallbackSlot;
use crate::hwtree::{HwTreeSource, NULL_HW_TREE};
use crate::input_focus::{InputFocus, NULL_INPUT_FOCUS};
use crate::spawn::{
    InitSpawn, ProcessSpawn, ProgramRegistry, EMPTY_PROGRAM_REGISTRY, NULL_PROCESS_SPAWN,
};
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

    /// Initial identity table to install during the `sec` init phase.
    ///
    /// Built from `/System/Security/Users` and `/System/Security/Groups`
    /// (or the installer-supplied bootstrap records on first boot). The builder
    /// is consumed and verified by [`crate::kernel_main`]; a rejected
    /// table aborts boot, (fail closed).
    pub identity: IdentityTableBuilder,

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

    /// The kernel input-focus arbiter the keyboard syscalls (`key_inject` /
    /// `display_acquire` / `display_release` / `keyboard_read`) drive
    /// (`plans/PI.md` P11 — input follows
    /// the surface owner).
    ///
    /// Defaults to [`NULL_INPUT_FOCUS`], whose text sink is the fail-closed
    /// [`crate::console::NULL_CONSOLE_INPUT`]: an arch port that has wired no
    /// arbiter leaves this default and a `key_inject` in the text focus fails
    /// closed rather than leaking a key edge to a device. A port installs its arbiter — its text sink pointed at the
    /// console that owns the directly attached keyboard — through
    /// [`Self::with_input_focus`]. Held as a `'static` borrow because the
    /// arbiter lives for the lifetime of the running kernel, exactly like the
    /// console list.
    pub input_focus: &'static InputFocus,

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
        identity: IdentityTableBuilder,
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
            identity,
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
            // Input-focus arbiter unwired until the arch port installs the
            // real one through `with_input_focus` (`plans/PI.md` P11):
            // `key_inject` / `keyboard_read` fail closed through
            // `NULL_INPUT_FOCUS`.
            input_focus: &NULL_INPUT_FOCUS,
            // Users database unwired until a boot path mounts the root
            // volume and installs the loaded holder through
            // `with_users_db` (`plans/PI.md` P11): `users_db_read` fails
            // closed through `NULL_USERS_DB`.
            users_db: &NULL_USERS_DB,
            // Hardware-tree store unwired until a boot path seeds the
            // discovered inventory and installs its store through
            // `with_hw_tree` (Design D): `hw_tree_read` / `hw_tree_wait`
            // fail closed through `NULL_HW_TREE`.
            hw_tree: &NULL_HW_TREE,
            _marker: core::marker::PhantomData,
        }
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

    /// Install the kernel input-focus arbiter the keyboard syscalls drive,
    /// consuming and returning `self` (`plans/PI.md` P11).
    ///
    /// Called by an arch port's boot pipeline after it has built the arbiter
    /// with its text sink pointed at the console that owns the directly
    /// attached keyboard (on the Pi, the video console's input queue). Until
    /// this is called the handover holds [`NULL_INPUT_FOCUS`] and a
    /// `key_inject` in the default text focus fails closed. The arbiter must be `'static`: the boot path leaks it alongside
    /// the kernel state, which lives for the lifetime of the running kernel
    /// (the install is a one-shot move, not a global
    /// mutable static).
    #[must_use]
    pub fn with_input_focus(mut self, input_focus: &'static InputFocus) -> Self {
        self.input_focus = input_focus;
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
            IdentityTableBuilder::new(),
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
            fn text(&self) -> Result<&[u8], Errno> {
                Ok(b"rustos-users-v1\n")
            }
        }

        // Default handover: `users_db_read` is inert.
        let b = fresh_boot_info();
        assert_eq!(b.users_db.text(), Err(Errno::NotImplemented));

        // After install the handover serves the holder's bytes.
        let held: &'static FixedUsersDb =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(FixedUsersDb));
        let b = fresh_boot_info().with_users_db(held);
        assert_eq!(b.users_db.text(), Ok(&b"rustos-users-v1\n"[..]));
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
