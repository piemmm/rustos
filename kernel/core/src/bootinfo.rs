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

use rustos_kernel_mem::BootMemoryMap;
use rustos_kernel_sched::{CpuId, SchedulerArch, SchedulerConfig};
use rustos_kernel_sec::IdentityTableBuilder;
use rustos_log::{Level, Sink};

use crate::dispatch_slot::DispatchCallbackSlot;

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
    /// Built from `/etc/rustos/users` and `/etc/rustos/groups` (or the
    /// installer-supplied bootstrap records on first boot). The builder
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
            _marker: core::marker::PhantomData,
        }
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
    use crate::test_arch::TestArch;
    use alloc::sync::Arc;
    use rustos_kernel_sched::SchedulerConfig;
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
