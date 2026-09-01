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

use tairix_arch_api::{ContextSwitch, CoreClock, CpuFeatures, PlatformEntropy, SecondaryBringup};

use crate::sched::{CpuId, SchedulerArch, SchedulerConfig};
use tairix_kalloc::FreeListAllocator;
use tairix_kernel_irq::{IrqController, IrqTable, UNSUPPORTED_CONTROLLER};
use tairix_kernel_mem::{BootMemoryMap, PhysAddr};
use tairix_log::{Level, Sink};

use crate::console::{ConsoleDevice, NO_CONSOLES};
use crate::dispatch_slot::DispatchCallbackSlot;
use crate::fs::{
    FilesystemService, LateIdentity, VolumeForest, VolumeService, NULL_FILESYSTEM,
    NULL_VOLUME_FOREST, NULL_VOLUME_SERVICE,
};
use crate::hwtree::{HwTreeSource, NULL_HW_TREE};
use crate::seat::{SeatRegistry, NULL_SEAT_REGISTRY};
use crate::spawn::{
    ArchImageBuilder, InitSpawn, ProgramRegistry, EMPTY_PROGRAM_REGISTRY, NULL_ARCH_IMAGE_BUILDER,
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
    /// [`tairix_arch_api::TaskContext`] the runtime owns, not in the
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

    /// Reset the whole machine now (a warm reboot).
    ///
    /// Invoked by an explicit, audited operator control action (the
    /// pre-boot Supervisor console's `reboot` command), never on an error
    /// path. On a port that can drive a firmware reset (PSCI `SYSTEM_RESET`
    /// on aarch64, the SBI System-Reset extension on riscv64, the x86 reset
    /// path) this **does not return** — the platform restarts. It is typed
    /// `()` rather than `!` precisely because a reset can *fail* or be
    /// *unsupported*: the default, and any port whose firmware refuses,
    /// **returns**, so the caller can report the failure and carry on (fail
    /// safe — a machine that cannot reset stays running rather than wedging).
    ///
    /// The default is unsupported: it returns immediately without touching
    /// hardware, so a port that has no reset channel (the host test arch)
    /// honestly reports "not supported" rather than silently pretending.
    fn reboot(&self) {}

    /// Power the machine off now (an orderly shutdown / halt of the
    /// platform).
    ///
    /// Invoked by an explicit, audited operator control action (the
    /// pre-boot Supervisor console's `poweroff` command), never on an error
    /// path. On a port that can drive a firmware power-off (PSCI
    /// `SYSTEM_OFF` on aarch64, the SBI System-Reset extension's shutdown on
    /// riscv64, the ACPI/QEMU power-off path on x86_64) this **does not
    /// return** — the platform powers down. Like [`Self::reboot`] it is
    /// typed `()` because power-off can be unsupported: the default, and any
    /// port whose firmware refuses, **returns** so the caller can report the
    /// failure and stay in control (fail safe).
    ///
    /// The default is unsupported: it returns immediately without touching
    /// hardware. A caller that must guarantee the CPU stops after an
    /// unsupported power-off falls back to [`Self::halt`] itself.
    fn poweroff(&self) {}

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

    /// The platform's secondary-CPU bring-up surface, when this port can
    /// start additional CPUs.
    ///
    /// [`crate::kernel_main`] drives it once per boot, after every init
    /// phase has succeeded (the scheduler, IRQ dispatch, and syscall hook
    /// are live) and after the secondary dispatch hand-off is published,
    /// asking for each dense CPU id in `1..cpu_count`. The port must have
    /// installed its secondary entry and stacks **before** handing over a
    /// `BootInfo` with `cpu_count > 1`; the started core performs its own
    /// per-CPU hardware init and joins the scheduler through
    /// [`crate::run_secondary`].
    ///
    /// The default is `None` — a single-CPU port (or the host test arch)
    /// simply has no bring-up surface, and `kernel_main` starts nothing.
    /// A port returning `Some` must fail closed inside
    /// [`SecondaryBringup::start_secondary`] for an unstartable id; a
    /// refused core is reported on the audit log and the system continues
    /// on the cores that are online.
    fn secondary_bringup(&self) -> Option<&dyn SecondaryBringup> {
        None
    }

    /// The port's non-maskable lockup-recovery handle, when it can drive a
    /// cross-CPU recovery signal (a reschedule IPI, or a directed attention
    /// interrupt).
    ///
    /// [`crate::kernel_main`] installs it into the watchdog once at boot so
    /// a detected lockup can be met with a best-effort recovery attempt.
    /// The default is `None` — a port with no recovery channel simply
    /// leaves hard-lockup recovery inert; the detector still reports every
    /// lockup loudly, and the recovery attempt is honestly recorded as
    /// `unsupported` (fail closed, never a silent no-op).
    fn watchdog_recovery(&self) -> Option<&'static (dyn tairix_arch_api::WatchdogArch + Sync)> {
        None
    }

    /// The port's **machine-takeover** handle, when it can hand the whole
    /// machine over to the pre-boot Supervisor's one-way destructive
    /// whole-RAM test (`plans/NEW-SUPERVISOR.md` §9): stop every other CPU,
    /// mask interrupts, stop the lockup watchdog, and run the sweep over
    /// physical RAM through the port's own direct/identity map (how a port
    /// reaches RAM directly is per-silicon — it is not required to drop the
    /// MMU).
    ///
    /// Because a takeover is irreversible (its only exits are reset /
    /// power-off), the Supervisor's `memtest` command reads this handle
    /// only *after* an explicit typed confirmation and a synchronous
    /// pre-jump audit. Like [`Self::watchdog_recovery`] the reference is
    /// `'static` and shared, so it is `Sync`.
    ///
    /// # Supervisor-only access
    ///
    /// This accessor is the **single** gate to the destructive takeover
    /// mechanism, and it is deliberately not callable from just any holder of
    /// a `&dyn KernelArch`: it demands a
    /// [`TakeoverGrant`](crate::supervisor_system::TakeoverGrant), a witness
    /// that can be minted only inside `crate::supervisor_system` — the
    /// confirmed, audited `memtest` path. No other kernel subsystem,
    /// driver, or userland caller can construct the grant, so none can obtain
    /// the [`MachineTakeover`](tairix_arch_api::MachineTakeover) handle or
    /// invoke its `unsafe` steps. A port implements this by handing back its
    /// own `'static` takeover static, which it keeps private so the handle is
    /// reachable *only* through this gated accessor.
    ///
    /// # Default
    ///
    /// The default returns [`None`]: a port that has not wired the takeover
    /// slice (`wasm32`, the host test arch, or a bare-metal port before its
    /// takeover mechanism lands) honestly reports "not supported", so the
    /// Supervisor stays in the REPL and changes nothing rather than
    /// half-tearing-down the machine (fail closed, never a panic).
    fn machine_takeover(
        &self,
        _grant: &crate::supervisor_system::TakeoverGrant,
    ) -> Option<&'static (dyn tairix_arch_api::MachineTakeover + Sync)> {
        None
    }

    /// The physical extent `(base, len_bytes)` of the **active console
    /// framebuffer** scan-out surface, or [`None`] when this port has no
    /// framebuffer console up (a serial-only boot, or a surface that is not
    /// in swept RAM).
    ///
    /// The one-way `memtest` takeover (`plans/NEW-SUPERVISOR.md` §9) uses it
    /// to keep the live scan-out surface **out** of the destructive whole-RAM
    /// sweep, so the progress display survives the run: a firmware/mailbox
    /// framebuffer sits in ordinary usable DRAM (the Raspberry Pi 4), and
    /// sweeping it would scribble over the very pixels the test is drawing
    /// through. A port whose framebuffer lives in reserved memory or MMIO
    /// (a kernel-`.bss` ramfb surface, a VESA/GOP aperture) may return
    /// [`None`]: excluding a range the sweep never reaches anyway is
    /// harmless, so the accessor need only report a surface that *is* in
    /// sweepable RAM.
    ///
    /// The default is [`None`] — a port with no framebuffer console, or none
    /// in swept RAM, excludes nothing.
    fn console_framebuffer(&self) -> Option<(PhysAddr, u64)> {
        None
    }

    /// The port's resolver that names a stuck controller line belonging to a
    /// kernel-internal source with no task binding — a chained/bespoke line
    /// the kernel services itself (the platform MSI multiplexer, the console
    /// UART) whose interrupt numbers the port discovered at runtime.
    ///
    /// [`crate::kernel_main`] installs it into the watchdog once at boot so a
    /// hard-lockup report can render `stuck_owner=<name>` for such a line
    /// instead of a bare `unbound`, which would hide that the pending line is
    /// (for example) the USB/PCIe MSI line a wedged CPU could not service.
    /// The default is `None` — a port with no kernel-internal enabled lines
    /// to name simply leaves such a line rendering as `unbound`.
    fn watchdog_line_names(
        &self,
    ) -> Option<&'static (dyn crate::watchdog::KernelInternalLines + Sync)> {
        None
    }

    /// The port's CPU-feature detector (the Arch HAL `cpufeatures` slice),
    /// when it can read its ISA-extension capability from the silicon.
    ///
    /// [`crate::kernel_main`] uses it to fold each core's detected feature set
    /// into the migration-safe common set delivered to every process
    /// ([`crate::cpuops`]); `detect` reads the *calling* CPU's ID registers,
    /// so the boot CPU folds its own set here and each secondary folds its own
    /// in [`crate::run_secondary`].
    ///
    /// The default is `None` — a port without the slice (the host test arch)
    /// simply contributes nothing, so the delivered common set stays empty and
    /// every process falls closed to the portable baseline (never a trap).
    fn cpu_features(&self) -> Option<&dyn CpuFeatures> {
        None
    }

    /// The port's live-core-frequency source (the Arch HAL `coreclock`
    /// slice), when it can read a counter that advances at the actual
    /// (DVFS-varying) core clock.
    ///
    /// [`crate::kernel_main`] enables it on the boot CPU and installs it into
    /// the per-CPU frequency estimator ([`crate::cpufreq`]); each secondary
    /// enables it as it comes up. The estimator samples the returned counter
    /// pair at the preemption tick and reports the live clock through the
    /// System Information API.
    ///
    /// The default is `None` — a port without the slice (the host test arch)
    /// simply contributes no frequency source, so the estimator reports no
    /// live frequency (fail closed, never a fabricated rate) and readers fall
    /// back to the discovered nominal frequency.
    fn core_clock(&self) -> Option<&dyn CoreClock> {
        None
    }

    /// The Tier-1 architecture identity of this port, or `None` for a
    /// port that is not a shippable target (the host test arch).
    ///
    /// Consumed once at boot to mint the [`tairix_abi::BootFacts`] record
    /// the ungated `boot_facts_get` syscall reports. There is **no default
    /// impl**: every port must state its identity explicitly, so a new
    /// port cannot silently ship reporting another architecture's name
    /// (fail closed — a `None` leaves the boot facts uninstalled).
    fn arch_id(&self) -> Option<tairix_abi::Arch>;

    /// The discovered model name of the boot CPU, or `None` when the
    /// port cannot derive one.
    ///
    /// Consumed once at boot, alongside [`Self::arch_id`], to mint the
    /// [`tairix_abi::BootFacts`] record: a `None` is installed as
    /// [`tairix_abi::CpuName::UNKNOWN`] and readers render their own
    /// fallback. There is **no default impl**: every port must state
    /// what it discovered (the x86_64 CPUID brand string, the aarch64
    /// `MIDR_EL1` decode, the riscv64 device-tree cpu `compatible`) or
    /// an explicit `None`, so a port cannot silently ship a fabricated
    /// or missing name.
    fn cpu_name(&self) -> Option<tairix_abi::CpuName>;

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
    /// constructs the [`tairix_kernel_irq::IrqTable`] with the
    /// returned `max_line` and threads the returned controller
    /// through every subsequent [`tairix_kernel_irq::IrqTable::fire`]
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
    /// [`tairix_kernel_irq::MaskError::Unsupported`] —
    /// [`tairix_kernel_irq::IrqTable::fire`] in turn surfaces
    /// [`tairix_kernel_irq::IrqError::ArchUnsupported`]. Arch ports
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
    /// place, so `msi_alloc` returns [`tairix_kernel_irq`]-style
    /// `NotImplemented`.
    #[must_use]
    fn msi_alloc_facility(
        &self,
    ) -> Option<&'static (dyn crate::devres::MsiAllocFacility + 'static)> {
        None
    }

    /// Hand the kernel core the architecture's **port-I/O producer** — the
    /// mechanism that issues one `in`/`out` against a legacy I/O port — so
    /// the capability-gated `port_read` / `port_write` traps have something
    /// to drive (`plans/TIMESYNC.md` TS-3). The reference must be `'static`
    /// and is read once, during [`crate::Phase::Syscall`].
    ///
    /// # Default
    ///
    /// The default returns [`None`]: only the x86 family has an I/O port
    /// space, so every other port leaves both traps failing closed with
    /// `NotImplemented` rather than carrying a stub that addresses nothing.
    #[must_use]
    fn port_io_facility(&self) -> Option<&'static (dyn crate::devres::PortIoFacility + 'static)> {
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
    /// [`tairix_abi::Errno::NotImplemented`].
    #[must_use]
    fn direct_phys_map(&self) -> Option<&'static (dyn tairix_kernel_mem::PhysMap + Sync)> {
        None
    }

    /// Build this port's **kernel remap window** and hand back the map the
    /// growable kernel heap assembles its regions in.
    ///
    /// The heap needs a virtually-contiguous run for every region it grows,
    /// and drawing one as a physically contiguous block welds the largest
    /// serviceable allocation to the frame allocator's contiguity order and
    /// fails on a fragmented pool while RAM is still free
    /// (`plans/FIX-KHEAP.md`). Assembling the run out of several chunks
    /// instead needs a range of kernel virtual addresses that resolves under
    /// *every* translation root — which only the port can arrange, because
    /// only it builds those roots. So the port reserves the window (drawing
    /// the shared sub-hierarchy from `frames` through `physmap`) and returns
    /// the neutral [`tairix_kernel_mem::KernelVirtMap`] over it; the rest is
    /// architecture-neutral and lives in `kernel/mem`.
    ///
    /// Called once, from [`crate::Phase::Mem`], immediately after the frame
    /// allocator exists and before the heap growth source is installed. The
    /// returned reference must be `'static`.
    ///
    /// # Default
    ///
    /// The default returns [`None`]: a port with no MMU (`wasm32`) or one
    /// that has not reserved a window leaves the kernel heap on its
    /// bootstrap region rather than growing (fail closed, never a panic).
    #[must_use]
    fn install_kernel_remap(
        arch: &'static Self,
        frames: &'static tairix_kernel_mem::FrameAllocator,
        physmap: &'static (dyn tairix_kernel_mem::PhysMap + Sync),
    ) -> Option<&'static dyn tairix_kernel_mem::KernelVirtMap>
    where
        Self: Sized,
    {
        let _ = (arch, frames, physmap);
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
    /// failing closed with [`tairix_abi::Errno::EntropyNotReady`].
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
    /// This is what makes TAIRiX a **fully preemptive** kernel: the scheduler dispatch loop calls
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
/// reference is shared (`+ Sync`) because [`tairix_kernel_irq::IrqTable::fire`]
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
    /// `mask` returns [`tairix_kernel_irq::MaskError::Unsupported`].
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
    /// Every [`tairix_kernel_mem::MemoryRegion`] of kind
    /// [`tairix_kernel_mem::RegionKind::Usable`] is genuinely free RAM —
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
    /// through `lib/log`'s [`tairix_log::log`]).
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
    /// [`tairix_abi::Errno::NotImplemented`]: an arch
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
    /// fails closed with [`tairix_abi::Errno::NotFound`] until the arch
    /// port installs a populated registry through [`Self::with_spawn`]. The
    /// program bytes are `'static` (the host-only `elf2rxe` build glue bakes
    /// them into the kernel image), so the registry lives
    /// for the running kernel's lifetime, exactly like the console device.
    pub programs: &'static ProgramRegistry,

    /// Architecture-specific image builder the deferred launch path drives
    /// to build a child's hardware-isolated address space *on the child's
    /// own task* (`plans/FIX-DESKTOP.md` §2.6.5). `kernel_main` captures it
    /// to build the boot-installed [`crate::spawn_services::SpawnServices`]
    /// bundle; it is not threaded into the syscall handler.
    ///
    /// Defaults to [`NULL_ARCH_IMAGE_BUILDER`], whose `build` fails closed
    /// with [`tairix_abi::Errno::NotImplemented`]: a port that has no image
    /// builder wired leaves this default and a deferred load fails the child
    /// closed rather than half-building a task. A port that can build a child
    /// address space installs its builder through [`Self::with_spawn`];
    /// spawning is *not* a privileged bypass — the child receives only its
    /// manifest∩user-grant authority. Held as a `'static` borrow, like the
    /// console device.
    pub image_builder: &'static (dyn ArchImageBuilder + 'static),

    /// The on-disk application store the `spawn` syscall resolves a
    /// non-embedded `…/<Name>.app/Run` path against (`plans/APPS.md`
    /// deliverable 8): the build's embedded app trust anchor plus the
    /// readiness latch the boot path resolves when the `/System` mount
    /// reaches a terminal state.
    ///
    /// Defaults to `None`: a port with no storage floor leaves it unset and
    /// a store-bundle spawn fails closed with
    /// [`tairix_abi::Errno::NotFound`], parking nothing. A port whose boot
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
    /// closed with [`tairix_abi::Errno::NotImplemented`]: a boot path that
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
    /// closed with [`tairix_abi::Errno::NotImplemented`] — a boot path
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
    /// [`tairix_abi::Errno::NotImplemented`]: a boot path that seeds no
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
    /// with [`tairix_abi::Errno::NotImplemented`]: a boot path that has not
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
    /// [`tairix_abi::Errno::NotFound`]. A boot path that mounts volumes
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
    /// closed with [`tairix_abi::Errno::NotImplemented`]. A boot path
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
    /// kernel policy — `tairix_users::system_accounts`) and installs it
    /// into this cell, so the system and service accounts resolve from
    /// first boot on every architecture, before any volume exists. The
    /// encrypted-root unlock later replaces the held table with the merge
    /// of that same compiled half and the on-disk human accounts.
    ///
    /// Defaults to `None`: a boot path (or host harness) that wires no
    /// identity cell gets no install, `kernel_main` threads the inert
    /// [`crate::syscalls::NULL_IDENTITY`] into the dispatch hook, and
    /// every credential resolution fails closed with
    /// [`tairix_abi::Errno::NotImplemented`]. A production port installs
    /// its cell through [`Self::with_spawn_identity`]. The default
    /// `spawn` (inherit) never consults it. Held `'static` because the
    /// cell lives for the running kernel's lifetime.
    pub spawn_identity: Option<&'static LateIdentity>,

    /// The binary's `#[global_allocator]` — the one kernel heap.
    ///
    /// `kernel/core` cannot own it (`#[global_allocator]` must be declared
    /// by the final binary), so every bin hands its allocator over here.
    /// The field is **required**, not an installable option, because two
    /// things depend on reaching the real heap and both were silently
    /// skippable while a bin had to remember a registration call: the
    /// growable-heap source ([`crate::kheap::install_frame_heap_source`]),
    /// without which the heap stays capped at its bootstrap region, and the
    /// live capacity the System Information API reports as
    /// [`tairix_abi::sysinfo::KernelMemoryStats::kernel_heap_bytes`].
    pub heap: &'static FreeListAllocator,

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
        heap: &'static FreeListAllocator,
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
            heap,
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
            image_builder: &NULL_ARCH_IMAGE_BUILDER,
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
            // Installed memory unknown until the boot path threads its
            // pre-carve platform total through `with_installed_memory`;
            // the boot facts stay uninstalled (fail closed) until then.
            installed_memory_bytes: 0,
            _marker: core::marker::PhantomData,
        }
    }

    /// Record the installed physical memory, in bytes, as the platform's
    /// boot memory source reports it before any kernel carve-outs,
    /// consuming and returning `self`.
    ///
    /// `kernel_main` mints the [`tairix_abi::BootFacts`] record from this
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
    /// closed with [`tairix_abi::Errno::NotImplemented`]. The list must
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
    /// [`NULL_ARCH_IMAGE_BUILDER`], so `spawn` fails closed
    /// ([`tairix_abi::Errno::NotFound`] / [`tairix_abi::Errno::NotImplemented`]).
    /// Both must be `'static`: the program bytes and the producer live for
    /// the lifetime of the running kernel, exactly like the console device
    /// (the install is a one-shot move, not a global
    /// mutable static).
    #[must_use]
    pub fn with_spawn(
        mut self,
        programs: &'static ProgramRegistry,
        image_builder: &'static (dyn ArchImageBuilder + 'static),
    ) -> Self {
        self.programs = programs;
        self.image_builder = image_builder;
        self
    }

    /// Install the on-disk application store the `spawn` syscall resolves
    /// non-embedded `…/<Name>.app/Run` paths against, consuming and
    /// returning `self` (`plans/APPS.md` deliverable 8).
    ///
    /// Called by a boot pipeline whose storage floor will publish (or
    /// explicitly give up on) the `/System` mount and resolve the store's
    /// readiness latch on every outcome. Until this is called a
    /// store-bundle spawn fails closed ([`tairix_abi::Errno::NotFound`]),
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
    /// call fails closed with [`tairix_abi::Errno::NotImplemented`].
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
    /// closed with [`tairix_abi::Errno::NotImplemented`], so userland sees an
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
    /// [`tairix_abi::Errno::NotFound`].
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
    /// [`tairix_abi::Errno::NotImplemented`].
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
    /// [`tairix_abi::Errno::NotImplemented`]; the default `spawn` (inherit)
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
    use tairix_log::Level;

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
            crate::test_heap::leak_heap().expect("host test heap"),
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
        use tairix_abi::Errno;

        // A test users-db source serving fixed bytes, leaked to `'static`
        // exactly as a production boot leaks its `HeldUsersDbSource`.
        struct FixedUsersDb;
        impl UsersDbSource for FixedUsersDb {
            fn text(&self) -> Result<crate::users::UsersDbText, Errno> {
                Ok(crate::users::UsersDbText::new(
                    b"tairix-users-v1\n".to_vec(),
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
            &b"tairix-users-v1\n"[..]
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
