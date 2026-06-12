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
//! `BootInfo` is part of the in-tree `abi-v1` surface (`AGENTS.md` §9):
//! the layout is frozen on release. Extensions ship as new versioned
//! types per `AGENTS.md` §2.4 (no interface creep) — never as silent
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
use crate::spawn::{
    InitSpawn, ProcessSpawn, ProgramRegistry, EMPTY_PROGRAM_REGISTRY, NULL_PROCESS_SPAWN,
};

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
/// * [`Self::halt`] **must not return**: per `AGENTS.md` §2 and the
///   Stage 2 deliverables, a panic or an unrecoverable init failure
///   parks the CPU forever and never silently resets. Real ports
///   typically loop on `hlt` / `wfi` / `wfe` with interrupts disabled.
/// * [`SchedulerArch::current_cpu`] returns the calling CPU's
///   identifier. Used by the panic handler when dumping context.
pub trait KernelArch: SchedulerArch {
    /// The Arch-HAL context-switch primitive (`AGENTS.md` §17.2 — the
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
    /// flaw into the `clock_get` syscall (`AGENTS.md` §5.4.5 — fail
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
    ///   controller (`AGENTS.md` §2.1 — one-shot publish).
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
    ///   code path is a defect (`AGENTS.md` §2.1 — one-shot publish);
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
        // (adding one would expand the public surface — `AGENTS.md`
        // §2.4); print the `max_line` and the controller's address
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
/// (`AGENTS.md` §5.4.3 — *validate every input*).
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
    /// that can vouch for this and is reviewed accordingly
    /// (`AGENTS.md` §1).
    pub memory_map: BootMemoryMap,

    /// Initial identity table to install during the `sec` init phase.
    ///
    /// Built from `/System/Security/Users` and `/System/Security/Groups`
    /// (or the installer-supplied bootstrap records on first boot). The builder
    /// is consumed and verified by [`crate::kernel_main`]; a rejected
    /// table aborts boot, per `AGENTS.md` §5.4.5 (fail closed).
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
    /// index against (`plans/PI.md` P6 / P11, `AGENTS.md` §10 / §16.4 /
    /// §20): index 0 the primary console (the detected display when
    /// present, else the first discovered UART), each further entry an
    /// independent console with its own session context (the UART beside
    /// an active video console).
    ///
    /// Defaults to the empty [`NO_CONSOLES`], so every console-backed
    /// stream access fails closed with
    /// [`rustos_abi::Errno::NotImplemented`] (`AGENTS.md` §2.9): an arch
    /// port that has not discovered a console leaves this default and the
    /// streams announce an inert interface rather than touching a device
    /// that does not exist. A port installs its discovered list through
    /// [`Self::with_consoles`]. The raw devices are installed here; the
    /// kernel-core init pipeline wraps each read half in
    /// [`crate::console::BlockingConsoleRead`] before handing the list to
    /// the syscall layer (`AGENTS.md` §20 — the backing owns blocking).
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
    /// program (`AGENTS.md` §17.2 / §17.4 — the arch-specific page-table /
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
    /// them into the kernel image, `AGENTS.md` §2.2), so the registry lives
    /// for the running kernel's lifetime, exactly like the console device.
    pub programs: &'static ProgramRegistry,

    /// Architecture-specific producer the `spawn` syscall drives to build a
    /// child's hardware-isolated address space and admit it as a runnable
    /// process (`plans/SPAWN.md` SP3).
    ///
    /// Defaults to [`NULL_PROCESS_SPAWN`], which fails closed with
    /// [`rustos_abi::Errno::NotImplemented`] (`AGENTS.md` §2.9): a port that
    /// has no runtime-spawn producer wired leaves this default and `spawn`
    /// announces an inert subsystem rather than half-building a task. A port
    /// that can build a child address space installs its producer through
    /// [`Self::with_spawn`]; spawning is *not* a privileged bypass — the
    /// child receives only its manifest∩user-grant authority (`AGENTS.md`
    /// §4, §16.5). Held as a `'static` borrow, like the console device.
    pub spawn_service: &'static (dyn ProcessSpawn + 'static),

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
    /// expressions (`AGENTS.md` §2.4 — no interface creep manifests as
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
            // console list through `with_consoles` (`AGENTS.md` §2.9 /
            // §5.4).
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
            _marker: core::marker::PhantomData,
        }
    }

    /// Install the discovered system console list the stream syscalls
    /// resolve descriptors against, consuming and returning `self`.
    ///
    /// Called by an arch port's boot pipeline after it has selected the
    /// console devices from the normalised hardware tree (`plans/PI.md`
    /// P6 / P11, `AGENTS.md` §18): index 0 the primary console (the
    /// detected display when present, else the first discovered UART),
    /// each further entry an independent console with its own session
    /// context. Until this is called the handover holds the empty
    /// [`NO_CONSOLES`] and every console-backed stream access fails
    /// closed with [`rustos_abi::Errno::NotImplemented`]. The list must
    /// be `'static`: the boot path leaks it alongside the kernel state,
    /// which lives for the lifetime of the running kernel (`AGENTS.md`
    /// §2.1 — the install is a one-shot move, not a global mutable
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
    /// the lifetime of the running kernel (`AGENTS.md` §2.1 — the install
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
    /// (`AGENTS.md` §2.1 — the install is a one-shot move, not a global
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

    /// Verify the SAFETY-INVARIANTs documented on each field.
    ///
    /// Called once at the top of [`crate::kernel_main`] before any
    /// subsystem init runs. Returns a [`BootInfoError`] if any
    /// invariant is violated; the caller logs it and halts.
    ///
    /// The intent is *release-safe* validation: every check is a cheap
    /// integer comparison, so we do not gate them on `debug_assertions`
    /// (`AGENTS.md` §2 — fail closed).
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
        // tests by `AGENTS.md` §2.9.
        alloc::boxed::Box::leak(alloc::boxed::Box::new(crate::test_sink::TestSink::new()))
    }

    fn leak_dispatch_slot() -> &'static DispatchCallbackSlot {
        // `Box::leak` mirrors the bin-crate convention: the slot
        // outlives every test, matching the `&'static` invariant the
        // production binary upholds with a `static`. `AGENTS.md` §2.9
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
