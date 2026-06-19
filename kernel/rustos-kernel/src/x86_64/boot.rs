//! Bare-metal boot pipeline for the x86_64 `rustos-kernel` binary.
//!
//! [`boot`] is the single entry point. It is called from each
//! binary's `extern "C" fn kernel_main(multiboot_info: u64)` after the
//! arch crate's [`rustos_arch_x86_64::entry`] trampoline has validated
//! the Multiboot2 magic. It performs the BSP bring-up
//! sequence the prompt for Stage 3a (c7-bin) lays out — Multiboot2 →
//! ACPI/MADT → `BootMemoryMap`; `X86_64Arch::new`; per-CPU
//! `percpu::init` → `preempt::init_local_preempt` →
//! `syscall_entry::init_local_syscalls`; install the fail-closed
//! syscall-dispatch callback **before** `syscall` is enabled — and
//! then hands a fully-validated [`rustos_kernel_core::BootInfo`] to
//! [`rustos_kernel_core::kernel_main`].
//!
//! # SAFETY-INVARIANTs
//!
//! Each step of [`boot`] is the unsafe shim into one of the
//! architecture port's audited primitives. The invariants the arch
//! crate documents on those primitives are upheld here:
//!
//! * `percpu::init(0)` runs exactly once with interrupts disabled
//!   (the boot trampoline leaves `IF` clear, and we never `sti`
//!   ourselves — `kernel_core::kernel_main` halts at the end of
//!   `BootCompleted`).
//! * `set_dispatch_callback` is invoked **before**
//!   `init_local_syscalls`, satisfying the trampoline's "callback
//!   installed before `syscall` is enabled" requirement (see
//!   `rustos_arch_x86_64::syscall_entry` rustdoc and AGENTS.md §5.4.5).
//! * `init_local_preempt`, `init_local_syscalls` and
//!   `set_cpu_id_for_lapic` run with `cpu_index = 0` on the BSP after
//!   `percpu::init(0)`, satisfying their per-call SAFETY contracts.
//! * The Multiboot2 pointer is dereferenced only through the
//!   audited `multiboot2::BootInfo::parse` validator, which bounds the
//!   slice by the leading `total_size` field.
//!
//! # No `unwrap` / `expect` / `panic!` in production paths
//!
//! AGENTS.md §2.9 forbids panics in production paths. Every fallible
//! step inside [`boot`] returns a [`BootError`]; the outer function
//! reports the failure through the log sink and halts the CPU
//! forever via [`rustos_arch_x86_64::kernel_arch::halt`]. The CPU
//! never returns to the trampoline (the boot stub assumes
//! `kernel_main` does not return — `boot.s` SAFETY-INVARIANT 7).

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use rustos_abi::SYSCALL_MAX_ARGS;
use rustos_arch_x86_64::acpi::{self, MadtEntry};
use rustos_arch_x86_64::apic::{IoApic, Lapic, VolatileIoApicMmio, VolatileLapicMmio};
use rustos_arch_x86_64::apic_timer::{self, PolledPit, Rdtsc};
use rustos_arch_x86_64::bootmemory;
use rustos_arch_x86_64::gdt::PerCpuGdt;
use rustos_arch_x86_64::irq as arch_irq;
use rustos_arch_x86_64::kernel_arch::{halt as arch_halt, X86_64Arch, X86_64ArchStorage};
use rustos_arch_x86_64::multiboot2::BootInfo as Mb2BootInfo;
use rustos_arch_x86_64::{fault, percpu, preempt, smp, syscall_entry};
use rustos_kernel_core::{kernel_main, BootInfo, IrqRouting};
use rustos_kernel_irq::IrqController;
use rustos_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind};
use rustos_kernel_sched_api::SchedulerConfig;
use rustos_kernel_sec::IdentityTableBuilder;
use rustos_log::{Event, EventId, Field, Level, Sink};

use crate::mem_map::carve_guard_arena_from_map;
use crate::stack_arena::{IdentityBlockStore, KTHREAD_STACK_ARENA};
use crate::x86_64::arch_wrapper::BinArch;
use crate::x86_64::dispatch::{production_dispatch, DISPATCH_SLOT};
use crate::x86_64::init_spawn::X86_64_INIT_SPAWN;
use crate::x86_64::ioapic_controller::IoApicController;
use crate::x86_64::serial_sink::COM1_CONSOLES;

/// `IA32_EFER` MSR number and its No-Execute-Enable bit (bit 11). Enabling
/// `NXE` lets the W^X No-Execute leaf bit the process-image builder sets on
/// data/rodata pages and the user stack be honoured rather than treated as a
/// reserved bit that faults the page-table walk (`AGENTS.md` §19.2).
const IA32_EFER: u32 = 0xC000_0080;
const EFER_NXE: u64 = 1 << 11;

// --- BSP boot configuration ----------------------------------------

/// LAPIC-timer period programmed during BSP bring-up.
///
/// 1 ms matches the value the existing `scheduler_stress_qemu` test
/// uses; consistency removes one source of "why is QEMU TCG behaving
/// differently here?" noise from the boot test (AGENTS.md §7 — no
/// flaky tests, no avoidable jitter). The timer is armed but no
/// callback is installed, so each tick is a no-op except for the EOI
/// — see `rustos_arch_x86_64::preempt::rustos_arch_x86_64_timer_dispatch`.
const PREEMPT_PERIOD_US: u32 = 1_000;

/// PIT calibration window. 10 ms is the universally-attested PIT
/// calibration period (the channel-2 reload fits in 16 bits up to
/// ~54 ms).
const PREEMPT_CALIBRATION_WINDOW_US: u32 = 10_000;

/// Per-CPU kernel-stack size in bytes.
///
/// 64 KiB matches the BSP bootstrap stack in `kernel/arch/x86_64::boot.s`.
/// The stack hosts the kernel side of a `syscall` transition (frame
/// layout in `syscall_entry::syscall_entry_stub`) and, in the QEMU
/// integration verticals, a full device-bring-up scenario driven
/// synchronously on the boot thread — including a filesystem `open`
/// that stages whole blocks through on-stack scratch buffers. The
/// earlier 16 KiB was marginal for that nested path; 64 KiB gives ample
/// headroom.
const KERNEL_STACK_BYTES: usize = 64 * 1024;

/// Number of logical CPUs the production `rustos-kernel` boot path
/// brings up. It runs **single-CPU** (it never drives the
/// `SecondaryBringup` HAL method — that handshake is proven by the QEMU
/// verticals), so every per-CPU backing here is sized to one slot
/// (`AGENTS.md` §24.1 — capacity matches the machine the caller actually
/// drives, not a baked-in `MAX_CPUS` ceiling). A future AP-bring-up
/// commit sizes this from the §18-discovered MADT processor count.
const BOOT_CPUS: usize = 1;

/// 16-byte-aligned kernel-stack slot. Matches the System V AMD64
/// ABI's 16-byte stack-alignment requirement at function entry.
#[repr(C, align(16))]
struct KernelStack([u8; KERNEL_STACK_BYTES]);

impl KernelStack {
    const ZERO: Self = Self([0; KERNEL_STACK_BYTES]);
}

/// Per-CPU kernel stack pool, sized to the [`BOOT_CPUS`] this binary
/// brings up (the BSP). A future AP-bring-up commit sizes it from the
/// §18-discovered CPU count (`AGENTS.md` §24.1) rather than re-introducing
/// a fixed ceiling.
///
/// AGENTS.md §2 — the only `static mut` in the bin crate, justified
/// in `README.md` as the per-CPU bootstrap-stack arena. Access is
/// exclusively through [`kernel_stack_top`], which derives a
/// disjoint pointer per `cpu_index`.
static mut KERNEL_STACKS: [KernelStack; BOOT_CPUS] = {
    const Z: KernelStack = KernelStack::ZERO;
    [Z; BOOT_CPUS]
};

/// Per-CPU GDT/IDT/IST arena the arch crate's [`percpu`] entry points
/// index, sized to [`BOOT_CPUS`] and published once by [`try_boot`]
/// before [`percpu::init`] (`AGENTS.md` §24.1).
static PER_CPU_STORAGE: percpu::PerCpuStorage<BOOT_CPUS> = percpu::PerCpuStorage::new();

/// Per-CPU `syscall`-entry TLS arena, sized to [`BOOT_CPUS`] and published
/// once by [`try_boot`] before [`syscall_entry::init_local_syscalls`].
static SYSCALL_TLS_STORAGE: syscall_entry::SyscallTlsStorage<BOOT_CPUS> =
    syscall_entry::SyscallTlsStorage::new();

/// One byte past the top of `KERNEL_STACKS[cpu_index]`.
///
/// `cpu_index < BOOT_CPUS` is the caller's responsibility; [`boot`]
/// satisfies that statically (it only calls with `0`).
fn kernel_stack_top(cpu_index: usize) -> u64 {
    debug_assert!(cpu_index < BOOT_CPUS);
    // SAFETY: `cpu_index < BOOT_CPUS` per the debug assert above
    // (production callers in this module guarantee the bound at the
    // call site too); `addr_of` reads the static's address without
    // creating a Rust reference.
    let base = unsafe { core::ptr::addr_of!(KERNEL_STACKS[cpu_index]) } as u64;
    base + core::mem::size_of::<KernelStack>() as u64
}

// --- Errors --------------------------------------------------------

/// Failure modes of [`boot`].
///
/// Stored as a single `enum` rather than a heap-allocated message
/// string so the boot log emits a stable, machine-readable `cause`
/// field (AGENTS.md §5.4.4).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BootError {
    /// The Multiboot2 record at the loader-supplied address could not
    /// be parsed.
    Multiboot2Parse,
    /// The Multiboot2 record contains no memory-map tag (BIOS path)
    /// and no UEFI memory-map tag.
    NoMemoryMap,
    /// The Multiboot2 record contains no RSDP tag — the boot test
    /// runs on a UEFI-discovered firmware, which is required to
    /// publish one through Multiboot2.
    NoRsdp,
    /// The RSDP bytes failed [`acpi::Rsdp::validate`].
    BadRsdp,
    /// No MADT was found by walking the (X|R)SDT.
    NoMadt,
    /// The MADT bytes failed [`acpi::Madt::parse`].
    BadMadt,
    /// No enabled-Processor-Local-APIC entry covered the BSP — every
    /// Multiboot2-published firmware does, so this is a fatal
    /// discovery defect.
    BspLapicMissing,
    /// [`percpu::PerCpuStorage::register`] refused the per-CPU arena
    /// (already registered — a boot-path defect).
    PercpuStorageRegister,
    /// `percpu::init` rejected the BSP.
    PercpuInit,
    /// [`syscall_entry::SyscallTlsStorage::register`] refused the per-CPU
    /// syscall-TLS arena (already registered — a boot-path defect).
    SyscallTlsStorageRegister,
    /// LAPIC-timer calibration against the PIT failed.
    TimerCalibration,
    /// `preempt::init_local_preempt` rejected the BSP.
    PreemptInit,
    /// `syscall_entry::init_local_syscalls` rejected the BSP.
    SyscallInit,
    /// `X86_64Arch::new` rejected the BSP triple.
    ArchInit,
    /// `BootInfo::new`/`validate` rejected the assembled hand-off.
    BootInfoInvalid,
    /// MADT advertised no IO-APIC. Every PCAT/UEFI platform RustOS
    /// supports publishes at least one; the absence is a fatal
    /// discovery defect.
    NoIoApic,
    /// The total IO-APIC pin count exceeded the reserved external-IRQ
    /// vector range (`0x30..=0xFE`, 207 vectors). Real platforms ship
    /// at most ~120 pins across all IO-APICs combined, so this is a
    /// pathological case.
    IrqVectorExhausted,
    /// `percpu::install_vector` rejected the external-IRQ IDT install.
    /// Surfaces a defect in the per-CPU bootstrap latch or an
    /// out-of-range vector.
    IrqIdtInstall,
    /// `percpu::install_vector` rejected the dedicated page-fault
    /// (`#PF`, vector 14) IDT install. Surfaces a defect in the per-CPU
    /// bootstrap latch.
    PageFaultIsrInstall,
    /// `percpu::install_tss_rsp0` rejected the ring-3-trap `RSP0`
    /// install. Surfaces a defect in the per-CPU bootstrap latch or an
    /// invalid kernel stack top.
    TssRsp0Install,
    /// The arch-crate routing publisher refused the `(gsi, vector)`
    /// pair. The only documented failure is `VectorAlreadyBound`,
    /// which means the boot pipeline tried to publish the same
    /// vector twice.
    IrqRoutingPublish,
    /// `IoApicController::program_pin` rejected the binding.
    IrqProgramPin,
    /// More than one CPU was about to be brought up on a part whose
    /// CPUID does not advertise an Invariant TSC. `RDTSC` is the
    /// x86_64 monotonic clock source, and without the invariant
    /// guarantee it may run at a P-state-dependent rate or drift
    /// between cores, so a migrated task could observe time going
    /// backwards. Rather than silently trust the contract the boot
    /// path fails closed (`AGENTS.md` §5.4.5, §19.1). A single-CPU
    /// boot is unaffected: one TSC is self-monotonic.
    TscNotInvariant,
}

impl BootError {
    /// Stable cause string for audit records (AGENTS.md §5.4.4).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Multiboot2Parse => "multiboot2_parse",
            Self::NoMemoryMap => "no_memory_map",
            Self::NoRsdp => "no_rsdp",
            Self::BadRsdp => "bad_rsdp",
            Self::NoMadt => "no_madt",
            Self::BadMadt => "bad_madt",
            Self::BspLapicMissing => "bsp_lapic_missing",
            Self::PercpuStorageRegister => "percpu_storage_register_failed",
            Self::PercpuInit => "percpu_init_failed",
            Self::SyscallTlsStorageRegister => "syscall_tls_storage_register_failed",
            Self::TimerCalibration => "timer_calibration_failed",
            Self::PreemptInit => "preempt_init_failed",
            Self::SyscallInit => "syscall_init_failed",
            Self::ArchInit => "arch_init_failed",
            Self::BootInfoInvalid => "bootinfo_invalid",
            Self::NoIoApic => "no_io_apic",
            Self::IrqVectorExhausted => "irq_vector_exhausted",
            Self::IrqIdtInstall => "irq_idt_install_failed",
            Self::PageFaultIsrInstall => "page_fault_isr_install_failed",
            Self::TssRsp0Install => "tss_rsp0_install_failed",
            Self::IrqRoutingPublish => "irq_routing_publish_failed",
            Self::IrqProgramPin => "irq_program_pin_failed",
            Self::TscNotInvariant => "tsc_not_invariant",
        }
    }
}

/// Audit event the boot pipeline emits on failure. Kept separate from
/// the `kernel/core` audit catalogue because the failure happens
/// *before* `kernel_core::kernel_main` is ever entered (and therefore
/// before its phase events have any meaning).
///
/// `EventId(4099)` sits in the `4000..5000` range owned by `kernel/core`
/// (per `lib/log`'s subsystem ranges) but at the top of the range so
/// it cannot collide with any phase-numbered event. The id is part of
/// the audit contract with external consumers and may not be renumbered
/// (`AGENTS.md` §5.4.4).
const KERNEL_BOOT_INIT_FAILED: EventId = EventId(4099);

/// Security-relevant boot decision: whether the BSP's CPUID advertises
/// an Invariant TSC (`AGENTS.md` §19.1). Logged on every boot so the
/// TSC contract is recorded rather than silently assumed. Sits in the
/// `kernel/core`-owned `4000..5000` range, just below
/// [`KERNEL_BOOT_INIT_FAILED`]; the id is part of the audit contract
/// and may not be renumbered (`AGENTS.md` §5.4.4).
const KERNEL_BOOT_TSC_INVARIANCE: EventId = EventId(4098);

/// Security-relevant boot decision: whether the kthread-stack guard arena
/// was carved from firmware-usable RAM and installed (`AGENTS.md` §4 — a
/// guarded per-task kernel stack whose guard page faults a stack overrun).
/// When no usable region can host a whole 2 MiB-aligned arena below the
/// identity window, the seam falls back to the software canary; logged on
/// every boot so the choice is audited, not assumed. Sits in the
/// `kernel/core`-owned `4000..5000` range, just below
/// [`KERNEL_BOOT_TSC_INVARIANCE`]; the id is part of the audit contract and
/// may not be renumbered (`AGENTS.md` §5.4.4).
const KERNEL_BOOT_GUARD_ARENA: EventId = EventId(4097);

/// Upper bound (exclusive) for the kthread-stack guard arena: the low
/// identity window the x86_64 spawn seams (`init_spawn_x86_64`,
/// `spawn_producer_x86_64`) build each task's root with (their
/// `IDENTITY_GIB` = 4 GiB). A stack outside it could not be reached — nor
/// its guard page faulted — under the task's own `CR3` (`AGENTS.md` §4), so
/// the arena carve refuses to place the arena there.
const KTHREAD_ARENA_IDENTITY_LIMIT: u64 = 4 << 30;

// --- The boot entry -------------------------------------------------

/// Boot the kernel on the BSP and forward to
/// [`rustos_kernel_core::kernel_main`].
///
/// `log_sink` and `audit_sink` are the `&'static` sinks installed in
/// [`rustos_kernel_core::BootInfo`]: the production binary uses a
/// COM1-backed sink for both; the QEMU integration test substitutes
/// the audit sink with one that flips the QEMU `isa-debug-exit`
/// device on `AuditEvent::BootCompleted`.
///
/// Returns the bottom type. On every failure the function logs one
/// [`KERNEL_BOOT_INIT_FAILED`] record (with the stable cause string
/// from [`BootError::as_str`]) and parks the CPU forever via
/// [`rustos_arch_x86_64::kernel_arch::halt`] — AGENTS.md §2 (fail
/// closed, no silent reset).
///
/// # SAFETY-INVARIANT
///
/// `multiboot_info` must be the verbatim 64-bit pointer the arch
/// crate's boot trampoline received in `%ebx`. `boot.s`
/// SAFETY-INVARIANT 7 documents that the pointer is in the
/// identity-mapped 0..4 GiB window.
pub fn boot(
    multiboot_info: u64,
    log_sink: &'static (dyn Sink + Sync),
    audit_sink: &'static (dyn Sink + Sync),
) -> ! {
    match try_boot(multiboot_info, log_sink, audit_sink) {
        Ok(boot_info) => kernel_main(boot_info),
        Err(err) => {
            log_init_failure(log_sink, err);
            arch_halt()
        }
    }
}

fn log_tsc_invariance(sink: &(dyn Sink + Sync), invariant: bool) {
    // Record the decision on every boot so the TSC contract is audited,
    // not silently trusted (`AGENTS.md` §19.1). A part that advertises
    // the invariant flag logs at Info; one that does not logs at Warn,
    // because a later SMP bring-up on it is refused (`try_boot`).
    let (level, message) = if invariant {
        (Level::Info, "tsc invariance validated")
    } else {
        (
            Level::Warn,
            "tsc not invariant; single-cpu boot proceeds, smp gated",
        )
    };
    rustos_log::log(
        sink,
        &Event {
            level,
            id: KERNEL_BOOT_TSC_INVARIANCE,
            message,
            fields: &[Field {
                key: "invariant_tsc",
                value: if invariant { "true" } else { "false" },
            }],
        },
    );
}

fn log_init_failure(sink: &(dyn Sink + Sync), err: BootError) {
    rustos_log::log(
        sink,
        &Event {
            level: Level::Error,
            id: KERNEL_BOOT_INIT_FAILED,
            message: "kernel boot init failed",
            fields: &[Field {
                key: "cause",
                value: err.as_str(),
            }],
        },
    );
}

fn try_boot(
    multiboot_info: u64,
    log_sink: &'static (dyn Sink + Sync),
    audit_sink: &'static (dyn Sink + Sync),
) -> Result<BootInfo<'static, BinArch>, BootError> {
    // 1. Per-CPU init (BSP).
    //
    //    Publish the caller-owned per-CPU GDT/IDT/IST arena before the
    //    first `percpu::init`, so the arch crate indexes a runtime-sized
    //    slice rather than a baked-in `MAX_CPUS` arena (`AGENTS.md`
    //    §24.1). `register` is set-once and `boot` runs once, so a second
    //    publish is a boot-path defect that fails closed.
    PER_CPU_STORAGE
        .register()
        .map_err(|_| BootError::PercpuStorageRegister)?;

    // SAFETY: This is the BSP, called exactly once. The boot
    // trampoline (`boot.s`) leaves `IF=0` so interrupts remain
    // disabled, satisfying `percpu::init`'s SAFETY contract.
    unsafe { percpu::init(0).map_err(|_| BootError::PercpuInit)? };

    // 1b. Overwrite the page-fault vector (`#PF`, 14) with the
    //     dedicated, error-code-aware entry. `percpu::init` populated
    //     every vector with the no-error default thunk, which mishandles
    //     the hardware error code the CPU pushes for `#PF`. The dedicated
    //     entry decodes the error code, captures the faulting address
    //     (`CR2`), and routes to the set-once `fault` observer — or, with
    //     none installed, preserves the exact fail-closed default
    //     (`AGENTS.md` §2.9 / §2.17 — no security regression). This makes
    //     a `#PF` correctly handled and observable on x86_64, the parity
    //     the riscv64/aarch64 `fault` hooks already have.
    //
    // SAFETY: BSP after `percpu::init(0)`; interrupts are still disabled
    // (the boot trampoline leaves `IF=0` and nothing has `sti`'d), so the
    // IDT write cannot race a delivery. `PAGE_FAULT_VECTOR` is neither
    // `#NMI` (2) nor `#DF` (8), so it does not disturb their IST routing.
    unsafe {
        percpu::install_vector(0, fault::PAGE_FAULT_VECTOR, fault::page_fault_isr_addr())
            .map_err(|_| BootError::PageFaultIsrInstall)?;
    }

    // 1c. Enable `IA32_EFER.NXE` so the W^X No-Execute leaf bit the
    //     process-image builder sets on a ring-3 program's data/rodata
    //     pages and its stack is honoured (`AGENTS.md` §19.2). Without it,
    //     bit 63 is reserved and the first non-executable user mapping the
    //     `init` spawn seam builds would fault the page-table walk. Enabling
    //     it on the BSP before any user image is built is the production W^X
    //     contract; it preserves `SCE`/`LME`/`LMA` the boot trampoline set.
    //
    // SAFETY: BSP after `percpu::init(0)`; interrupts disabled. The
    // read-modify-write only sets bit 11, leaving every other `IA32_EFER`
    // bit (long-mode enable/active, syscall enable) intact.
    unsafe {
        enable_nxe();
    }

    // 2. Software-enable the BSP LAPIC and read its ID.
    let mut lapic = make_bsp_lapic();
    lapic.software_enable(0xFF);
    let bsp_lapic_id = smp::bsp_lapic_id();

    // 3. Calibrate the LAPIC timer against the PIT. The same window
    //    samples RDTSC so the resulting `Calibration::tsc_per_second`
    //    is the unit input to `BinArch::monotonic_ns` (the production
    //    `clock_get` syscall path, Stage 2.7 follow-up (f3)).
    let mut pit = PolledPit;
    let mut tsc = Rdtsc;
    let calibration = apic_timer::calibrate(
        &mut lapic,
        &mut pit,
        &mut tsc,
        PREEMPT_CALIBRATION_WINDOW_US,
        PREEMPT_PERIOD_US,
    )
    .map_err(|_| BootError::TimerCalibration)?;

    // 4. Multiboot2 parsing — first the memory map, then the RSDP.
    let mb2 = parse_multiboot2(multiboot_info)?;

    let mut memory_map = build_memory_map(&mb2)?;

    // Carve a 2 MiB-aligned kthread-stack guard arena out of the firmware
    // map and install it so the PID 1 spawn seam (`init_spawn_x86_64`) can
    // draw `init`'s kernel stack from it and unmap that stack's guard page
    // in `init`'s own `CR3` — turning a stack overrun into a synchronous
    // fault rather than a poison-canary detection (`plans/PI.md` G3b-2, the
    // cross-port sibling of the aarch64 `boot_aarch64` wiring). The arena is
    // sized from the discovered *usable* RAM (§24.2 policy) and bounded to
    // the seams' 4 GiB identity window so the stack is reachable under the
    // task's own root. When no usable region fits a whole arena the carve
    // returns `None`, the install is skipped, and the seam falls back to a
    // software-canary `BoxStack` (fail closed, never fatal to boot,
    // `AGENTS.md` §2.9 / §2.17).
    //
    // The policy input is the sum of `Usable` region lengths, *not*
    // `highest_address()`: a PC firmware map spans the reserved MMIO/PCI hole
    // up to (and past) 4 GiB, so the highest address wildly over-states RAM
    // and would always saturate the arena to its 64 MiB cap. Summing usable
    // bytes (after the kernel-image reservation) is the RAM actually
    // available, the aarch64 single-window sizing's multi-region analogue.
    let ram_bytes: u64 = memory_map
        .regions()
        .iter()
        .filter(|region| region.kind == RegionKind::Usable)
        .fold(0u64, |acc, region| acc.saturating_add(region.length));
    let guard_arena =
        carve_guard_arena_from_map(&mut memory_map, ram_bytes, KTHREAD_ARENA_IDENTITY_LIMIT);
    if let Some(arena) = guard_arena {
        KTHREAD_STACK_ARENA.install(arena.base, arena.len, &IdentityBlockStore);
    }
    crate::mem_map::log_guard_arena(
        log_sink,
        KERNEL_BOOT_GUARD_ARENA,
        guard_arena.map(|a| (a.base, a.len)),
    );

    let rsdp_bytes = mb2.rsdp().ok_or(BootError::NoRsdp)?;
    let rsdp = acpi::Rsdp::validate(rsdp_bytes).map_err(|_| BootError::BadRsdp)?;

    // 5. MADT walk → BSP LAPIC verification.
    //
    // SAFETY: `rsdp` was validated above; its XSDT/RSDT pointers came
    // from firmware-published tables in the identity-mapped 0..4 GiB
    // window (`boot.s` SAFETY-INVARIANT 4).
    let madt_bytes = unsafe { acpi::locate_madt(&rsdp) }.ok_or(BootError::NoMadt)?;
    let madt = acpi::Madt::parse(madt_bytes).map_err(|_| BootError::BadMadt)?;
    verify_bsp_present(&madt, bsp_lapic_id)?;

    // 6. Build the `cpu_to_lapic` map with **only** the BSP populated.
    //    Production `rustos-kernel` runs single-CPU (it never calls the
    //    `SecondaryBringup` HAL method — that handshake is proven by the
    //    QEMU verticals), so the arch handle's per-CPU bookkeeping is
    //    sized to one slot (`AGENTS.md` §24.1 — capacity matches the
    //    machine the caller actually drives, no global `MAX_CPUS`
    //    ceiling baked into the arch crate). The per-CPU kernel-stack
    //    pool keeps its own `MAX_CPUS` secondary-bring-up bound.
    let cpu_to_lapic: [Option<u8>; 1] = [Some(bsp_lapic_id)];

    // 6b. Validate the TSC before trusting `RDTSC` as the cross-CPU
    //     monotonic clock source. The contract is recorded on every
    //     boot rather than silently assumed (`AGENTS.md` §19.1). A
    //     single-CPU boot proceeds regardless — one TSC is inherently
    //     self-monotonic — but the day this pipeline brings up a
    //     second CPU on a part without an Invariant TSC, it fails
    //     closed instead of risking a non-monotonic `clock_get`
    //     (`AGENTS.md` §5.4.5). The CPUID probe lives in the arch crate
    //     per §17.2.
    let invariant_tsc = rustos_arch_x86_64::tsc::detect_invariant_tsc();
    log_tsc_invariance(log_sink, invariant_tsc);
    let active_cpu_count = cpu_to_lapic.iter().filter(|slot| slot.is_some()).count();
    if !invariant_tsc && active_cpu_count > 1 {
        return Err(BootError::TscNotInvariant);
    }

    // The arch handle borrows its per-CPU bookkeeping from this
    // process-static backing (`AGENTS.md` §24.1); `boot` runs once, so a
    // single `static` is sound and needs no allocator.
    static ARCH_STORAGE: X86_64ArchStorage<1> = X86_64ArchStorage::new();
    let arch = X86_64Arch::new(&ARCH_STORAGE, 0, bsp_lapic_id, &cpu_to_lapic)
        .map_err(|_| BootError::ArchInit)?;

    // 7. Install the production syscall-dispatch callback **before**
    //    `init_local_syscalls` enables `syscall` on any CPU. The
    //    ordering matters per `syscall_entry` rustdoc — the trampoline
    //    fail-closes if it fires with no callback installed.
    //
    //    Stage 2.7 follow-up (f5). `production_dispatch` reads the
    //    `DISPATCH_SLOT` static (whose hook `kernel_main` publishes
    //    during the `Syscall` init phase between Sched and Ipc) and
    //    forwards every syscall through the resident `DispatchHook`.
    //    If a syscall fires before the slot is published, or if the
    //    hook signals `NoCallerContext`, the callback halts the CPU
    //    forever — the same fail-closed posture the (c7-bin) commit
    //    shipped, now coexisting with the live dispatcher.
    syscall_entry::set_dispatch_callback(production_dispatch);

    // 7b. Publish the caller-owned per-CPU syscall-TLS arena before
    //     `init_local_syscalls` (which writes this CPU's slot and points
    //     `IA32_KERNEL_GS_BASE` at it). Runtime-sized, set-once, fails
    //     closed on a second publish (`AGENTS.md` §24.1 / §2.9).
    SYSCALL_TLS_STORAGE
        .register()
        .map_err(|_| BootError::SyscallTlsStorageRegister)?;

    // 8. Install the LAPIC timer ISR + program the period. No timer
    //    callback is registered: the timer ISR's null-callback branch
    //    issues the EOI and returns, which is exactly what we want
    //    until the scheduler dispatch loop lands in Stage 2.7.
    //
    // SAFETY: this is the BSP whose `percpu::init(0)` ran above,
    // interrupts are disabled, and `lapic` is the BSP's LAPIC because
    // it was constructed from the architectural LAPIC base.
    unsafe {
        preempt::init_local_preempt(0, &mut lapic, calibration)
            .map_err(|_| BootError::PreemptInit)?;
    }

    // 9. Populate the LAPIC→CpuId mapping so the timer ISR can
    //    translate the LAPIC ID register reading to a dense CpuId.
    preempt::set_cpu_id_for_lapic(bsp_lapic_id, 0);

    // 10. Enable `syscall`/`sysret` on the BSP. The callback is
    //     already installed (step 7) and the kernel stack top is
    //     provided by `kernel_stack_top`.
    let sel = PerCpuGdt::selectors();
    // `STAR[63:48]` is the "sysret user base"; on `sysretq` long mode
    // the CPU loads `CS = base + 16`, `SS = base + 8`. See
    // `syscall_entry::encode_star` rustdoc.
    let sysret_user_base = sel.user_cs - 16;
    let kernel_rsp0 = kernel_stack_top(0);
    // SAFETY: BSP after `percpu::init(0)`; interrupts disabled; the
    // kernel stack top is one byte past a 16-KiB, 16-byte-aligned
    // backing region; the dispatch callback was installed above.
    unsafe {
        syscall_entry::init_local_syscalls(0, sel.kernel_cs, sysret_user_base, kernel_rsp0)
            .map_err(|_| BootError::SyscallInit)?;
    }

    // 10a. Install the TSS `RSP0` the CPU loads on a ring-3 -> ring-0 CPU
    //      exception or hardware interrupt. `init_local_syscalls` programs
    //      the *syscall* entry stack (loaded via `swapgs`), but a ring-3
    //      `#PF`/`#GP` or a timer IRQ that preempts a user task is delivered
    //      through the IDT, for which the CPU reads `TSS.RSP0`. Left zero
    //      (the `percpu::init` default), the interrupt-frame push faults and
    //      escalates to `#DF`, so a user trap is *undeliverable* — a security
    //      gap, not a feature (`AGENTS.md` §2.9 / §2.17). The trap stack is
    //      the same already-mapped per-CPU kernel stack the syscall path uses
    //      (Linux likewise shares one kernel stack for syscalls and traps):
    //      `RSP0` is only loaded on a ring-3 -> ring-0 transition, when that
    //      stack is idle, so there is no overlap with an in-flight syscall.
    //
    // SAFETY: BSP after `percpu::init(0)`; interrupts disabled; `kernel_rsp0`
    // is the validated top of the 16-KiB, 16-byte-aligned per-CPU kernel
    // stack, mapped in every address space this CPU runs.
    unsafe {
        percpu::install_tss_rsp0(0, kernel_rsp0).map_err(|_| BootError::TssRsp0Install)?;
    }

    // 10b. Stage 4.D Item 2-tail.2: discover every IO-APIC the MADT
    //      advertises, allocate one external-IRQ vector per pin,
    //      install the per-pin IDT entry, populate the arch crate's
    //      lock-free routing table, and program the redirection entry
    //      `masked = true`. The driver-host side (Item 2-tail.3,
    //      out of scope here) will later unmask each line through the
    //      controller's `program_pin` re-publish path when a driver
    //      binds to the GSI.
    let irq_routing = discover_and_program_io_apics(&madt, bsp_lapic_id)?;

    // 11. Assemble the `BootInfo` and hand off to `kernel_core`.
    //
    // Build the `Arc<BinArch>` ahead of the `BootInfo::new` call so we
    // can publish the pointer into `panic_ctx::PANIC_ARCH_PTR` for the
    // panic-handler bridge. The `Arc` is kept alive by `BootInfo`'s
    // `arch` field (and re-cloned into `kernel_core`'s `KernelState`),
    // so the published pointer remains valid for the lifetime of the
    // running kernel.
    let arch_arc: Arc<BinArch> = Arc::new(BinArch::new(arch, calibration, irq_routing));
    // SAFETY: `arch_arc` is moved into `BootInfo` immediately below
    // (which `kernel_main` consumes and stores). `Arc::as_ptr` returns
    // a stable pointer for the lifetime of any clone of the `Arc`.
    unsafe {
        crate::x86_64::panic_ctx::publish_arch(Arc::as_ptr(&arch_arc));
    }
    // Publish a clone of the firmware memory map into the bin-crate's
    // set-once slot before it is moved into the `kernel_core` hand-off,
    // so a driver-bring-up observer can build a per-device DMA
    // `FrameAllocator` from the same firmware description without
    // re-borrowing the `pub(crate)` `KernelState` (AGENTS.md §2.1).
    crate::x86_64::arch_wrapper::publish_memory_map(&memory_map);

    let scheduler_config = SchedulerConfig::defaults_for(1);
    let boot_info: BootInfo<'static, BinArch> = BootInfo::new(
        /* boot_cpu       = */ 0,
        /* cpu_count      = */ 1,
        /* command_line   = */ "",
        memory_map,
        IdentityTableBuilder::new(),
        scheduler_config,
        arch_arc,
        log_sink,
        audit_sink,
        Level::Info,
        // Stage 2.7 follow-up (f4): hand the bin-crate-owned slot to
        // `kernel_main`'s `Syscall` phase. The arch-level
        // `set_dispatch_callback` (step 7 above) is unchanged; this
        // is the *kernel-side* publication point for the eventual
        // production dispatch hook.
        &DISPATCH_SLOT,
    )
    // The COM1 console list for the standard streams: `stream_write` on
    // fd 1/2/3 reaches the same serial line the log sink uses, so PID 1
    // `init`'s banner lands (`plans/PI.md` X3a, `AGENTS.md` §20). It is a
    // stream *backing*, not a program-facing device; the read half fails
    // closed (no COM1 RX drain is wired on this slice).
    .with_consoles(&COM1_CONSOLES)
    // The PID 1 (`init`) spawn seam: after `BootCompleted`, `kernel_main`
    // builds `init`'s ring-3 image and drops into it as a resumable user
    // kthread (`plans/PI.md` X3a).
    .with_init(&X86_64_INIT_SPAWN)
    // The runtime `spawn` producer + embedded-program registry
    // (`plans/PI.md` X3b): the `spawn` syscall resolves a path against the
    // registry and drives the producer to build a fresh, isolated child PML4,
    // so PID 1 `init` can launch the user's session concurrently — the
    // cross-port sibling of the aarch64 `boot_aarch64` wiring.
    .with_spawn(
        &crate::spawn_layout::PROGRAM_REGISTRY,
        &crate::x86_64::spawn_producer::X86_64_PROCESS_SPAWN,
    )
    // Serve the discovered hardware tree (`AGENTS.md` §18.1 / §18.4): the
    // `hw_tree_read` / `hw_tree_wait` syscalls read the one authoritative
    // `HW_TREE`, so the user-space device manager observes the same
    // inventory the kernel discovered (Design D).
    .with_hw_tree(&crate::hwtree_store::HW_TREE_SOURCE);
    boot_info
        .validate()
        .map_err(|_| BootError::BootInfoInvalid)?;

    // The caller forwards to `kernel_main`, which returns `!` and
    // never re-enters this function.
    Ok(boot_info)
}

/// Enable the No-Execute-Enable bit in `IA32_EFER` on the current CPU.
///
/// # Safety
///
/// Must run in ring 0 with interrupts disabled (the BSP after
/// `percpu::init`). Performs a `rdmsr`/`wrmsr` read-modify-write that only
/// sets [`EFER_NXE`], preserving every other `IA32_EFER` bit.
unsafe fn enable_nxe() {
    let lo: u32;
    let hi: u32;
    // SAFETY: `rdmsr` of `IA32_EFER` is well-defined in ring 0; it has no
    // memory effects and clobbers only the named registers.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") IA32_EFER,
            out("eax") lo,
            out("edx") hi,
            options(nostack, preserves_flags),
        );
    }
    let efer = (((hi as u64) << 32) | lo as u64) | EFER_NXE;
    // SAFETY: writing `IA32_EFER` back with only bit 11 newly set is the
    // documented enable sequence; `SCE`/`LME`/`LMA` are preserved.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") IA32_EFER,
            in("eax") efer as u32,
            in("edx") (efer >> 32) as u32,
            options(nostack, preserves_flags),
        );
    }
}

fn make_bsp_lapic() -> Lapic<VolatileLapicMmio> {
    // SAFETY: `LAPIC_BASE_PHYS` (= 0xFEE0_0000) is identity-mapped by
    // `boot.s` SAFETY-INVARIANT 4 (the 0..4 GiB identity map covers
    // it). The constructor only stores the pointer; no MMIO read or
    // write happens here.
    let mmio = unsafe { VolatileLapicMmio::new(preempt::LAPIC_BASE_PHYS as *mut u32) };
    Lapic::new(mmio)
}

fn parse_multiboot2(multiboot_info: u64) -> Result<Mb2BootInfo<'static>, BootError> {
    // The Multiboot2 record's first 4 bytes are `total_size`. We read
    // it from the identity-mapped window, then bound the whole slice
    // by that length before handing it to `BootInfo::parse`, which
    // re-validates structure.
    //
    // SAFETY: `multiboot_info` is the verbatim 64-bit pointer from
    // `boot.s` SAFETY-INVARIANT 7; the first eight bytes are
    // accessible through the identity map.
    let header = unsafe { core::slice::from_raw_parts(multiboot_info as *const u8, 8) };
    let total_size = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    // SAFETY: same as above; `total_size` is the bootloader's stated
    // length of its own record, which the validator
    // (`BootInfo::parse`) re-bounds.
    let bytes = unsafe { core::slice::from_raw_parts(multiboot_info as *const u8, total_size) };
    Mb2BootInfo::parse(bytes).map_err(|_| BootError::Multiboot2Parse)
}

fn build_memory_map(mb2: &Mb2BootInfo<'_>) -> Result<BootMemoryMap, BootError> {
    let mut map = BootMemoryMap::new();

    if let Some(uefi) = mb2.efi_memory_map() {
        for desc in bootmemory::iter_from_uefi(&uefi) {
            push_descriptor(&mut map, desc);
        }
    } else if let Some(bios) = mb2.memory_map() {
        for desc in bootmemory::iter_from_multiboot2(&bios) {
            push_descriptor(&mut map, desc);
        }
    } else {
        return Err(BootError::NoMemoryMap);
    }

    // Reserve the running kernel image (boot trampoline through the end of
    // .bss, which includes the bump heap) out of the firmware-usable RAM.
    //
    // GRUB loads this multiboot2 kernel into memory the UEFI map reports as
    // `EfiLoaderData`/`EfiConventionalMemory` — both of which
    // `bootmemory::from_uefi` (correctly, post-`ExitBootServices`) classifies
    // `Usable`. Without this carve-out the frame allocator eventually hands
    // out frames overlapping the running kernel's code and heap, and the
    // `spawn` image builder's zero-fill / page-table writes corrupt the live
    // kernel (`plans/PI.md` X4 follow-on). This is the x86_64 sibling of the
    // aarch64 `mem_map` `[ram_base, __kernel_end)` reservation (`plans/PI.md`
    // P6c-1) — without it nothing protected the kernel image (`AGENTS.md`
    // §2.17, §4).
    let (kstart, kend) = kernel_image_phys_bounds();
    map.reserve_range(kstart, kend);

    Ok(map)
}

/// Physical `[start, end)` bounds of the running kernel image — the boot
/// trampoline (`__boot_phys_start`, fixed at 1 MiB) through the end of `.bss`
/// (`__kernel_phys_end`, which the linker emits as a *physical* address; see
/// `kernel/arch/x86_64/linker.ld`). The bump heap lives in `.bss`, so the
/// range covers it too.
fn kernel_image_phys_bounds() -> (PhysAddr, PhysAddr) {
    // `__boot_phys_start` / `__kernel_phys_end` are absolute symbols the
    // linker script defines; only their *addresses* (i.e. their linked
    // values) are read here — they are never dereferenced, so taking the
    // address is safe.
    let start = core::ptr::addr_of!(__boot_phys_start) as u64;
    let end = core::ptr::addr_of!(__kernel_phys_end) as u64;
    (PhysAddr::new(start), PhysAddr::new(end))
}

extern "C" {
    /// Physical start of the kernel image (the boot trampoline at 1 MiB).
    /// Defined by `kernel/arch/x86_64/linker.ld`.
    static __boot_phys_start: u8;
    /// Physical one-past-the-end of the kernel image (end of `.bss`,
    /// including the bump heap). Defined by `kernel/arch/x86_64/linker.ld`
    /// as `. - KERNEL_VMA_BASE`, so its linked value is a physical address.
    static __kernel_phys_end: u8;
}

fn push_descriptor(map: &mut BootMemoryMap, desc: bootmemory::MemoryRegionDescriptor) {
    // Translate the arch-port mirror enum into the kernel/mem
    // canonical enum. `bootmemory`'s host-side round-trip test pins
    // the two enums together at compile time so a future drift fails
    // the build (AGENTS.md §2.2).
    let kind = match desc.kind {
        bootmemory::RegionKind::Usable => RegionKind::Usable,
        bootmemory::RegionKind::Reserved => RegionKind::Reserved,
    };
    map.push(MemoryRegion {
        start: PhysAddr::new(desc.start),
        length: desc.length,
        kind,
    });
}

/// Discover every IO-APIC the MADT advertises, build a production
/// [`IoApicController`], install one per-pin IDT vector + routing
/// entry, and program every redirection entry masked.
///
/// Returns the [`IrqRouting`] the caller stores in [`BinArch`].
///
/// # Failure modes
///
/// * [`BootError::NoIoApic`] if MADT advertises none.
/// * [`BootError::IrqVectorExhausted`] if the total pin count exceeds
///   the reserved vector range (`0x30..=0xFE`, 207 vectors).
/// * [`BootError::IrqIdtInstall`] if a per-pin
///   [`percpu::install_vector`] call fails — pathological, the BSP
///   has finished `percpu::init` by this point.
/// * [`BootError::IrqRoutingPublish`] if the arch-crate routing
///   table refused a `(gsi, vector)` pair. The only documented
///   failure is `VectorAlreadyBound`, which would mean the boot
///   pipeline tried to publish the same vector twice.
/// * [`BootError::IrqProgramPin`] if the controller's
///   [`IoApicController::program_pin`] refused a binding.
fn discover_and_program_io_apics(
    madt: &acpi::Madt<'_>,
    bsp_lapic_id: u8,
) -> Result<IrqRouting, BootError> {
    // Step 1. Discover every IO-APIC entry. Each entry carries the
    // identification, the physical MMIO base address, and the GSI
    // base the chip owns. We do not yet read `max_redirection_entry`
    // — that requires a live `IoApic<M>` instance, which we build
    // below.
    struct Discovered {
        gsi_base: u32,
        mmio_base: u32,
        pin_count: u32,
    }
    let mut discovered: Vec<Discovered> = Vec::new();
    for entry in madt.entries() {
        if let MadtEntry::IoApic {
            address, gsi_base, ..
        } = entry
        {
            // SAFETY: the IO-APIC MMIO base addresses MADT publishes
            // sit at firmware-fixed physical frames covered by
            // `boot.s` SAFETY-INVARIANT 4 (0..4 GiB identity map).
            // The constructor only stores the pointer; no MMIO
            // access happens here.
            let mmio = unsafe { VolatileIoApicMmio::new(address as *mut u32) };
            let mut ioapic = IoApic::new(mmio);
            let pin_count = u32::from(ioapic.max_redirection_entry()) + 1;
            discovered.push(Discovered {
                gsi_base,
                mmio_base: address,
                pin_count,
            });
        }
    }
    if discovered.is_empty() {
        return Err(BootError::NoIoApic);
    }

    // Step 2. Pre-validate the total pin count against the reserved
    // vector range so we fail-closed before any IDT mutation.
    let total_pins: u32 = discovered.iter().map(|d| d.pin_count).sum();
    if total_pins as usize > arch_irq::EXTERNAL_VECTOR_COUNT {
        return Err(BootError::IrqVectorExhausted);
    }

    // Step 3. Construct the controller. Each block needs a fresh
    // `IoApic<M>` instance (the discovery instance above is dropped);
    // the controller takes ownership and serialises every subsequent
    // MMIO access through an internal `SpinLock`.
    let blocks: Vec<(u32, IoApic<VolatileIoApicMmio>, u32)> = discovered
        .iter()
        .map(|d| {
            // SAFETY: same as the discovery pass.
            let mmio = unsafe { VolatileIoApicMmio::new(d.mmio_base as *mut u32) };
            (d.gsi_base, IoApic::new(mmio), d.pin_count)
        })
        .collect();
    let controller_static: &'static IoApicController<VolatileIoApicMmio> =
        Box::leak(Box::new(IoApicController::new(blocks)));
    // Publish the typed controller into the bin-crate's `PUBLISHED_TYPED`
    // slot so in-kernel observers (e.g. the
    // `tests/integration/irq_qemu_x86_64` QEMU integration test) can
    // reach [`IoApicController::program_pin`] and
    // [`IoApicController::read_pin_low`] without re-borrowing the
    // `pub(crate)` `KernelState`. AGENTS.md §2.1 — one-shot publish;
    // the slot accepts the same pointer the `IrqRouting` carries.
    crate::x86_64::ioapic_controller::publish_typed(controller_static);

    // Step 4. For every pin: allocate the next vector from the
    // reserved range, install the per-CPU IDT entry, publish the
    // `(gsi, vector)` pair into the arch crate's routing table,
    // and program the IO-APIC redirection entry `masked = true`
    // so no line fires until a driver explicitly unmasks it.
    let routing = arch_irq::global_routing();
    let mut next_vector: u8 = arch_irq::EXTERNAL_VECTOR_FIRST;
    let mut max_gsi: u32 = 0;
    for d in &discovered {
        for pin_offset in 0..d.pin_count {
            let gsi = d.gsi_base + pin_offset;
            if next_vector > arch_irq::EXTERNAL_VECTOR_LAST {
                return Err(BootError::IrqVectorExhausted);
            }
            let vector = next_vector;
            // Saturating-add is sufficient: once `next_vector` lands
            // on `0xFF` the loop's bound check above fails on the
            // following iteration.
            next_vector = next_vector.saturating_add(1);

            // SAFETY: `vector` is in `EXTERNAL_VECTOR_FIRST..=LAST`
            // by the bound check; `external_isr_addr` returns `Some`
            // for every value in that range (the per-vector stub
            // table in `external_irq.s` is dense).
            let isr_addr = arch_irq::external_isr_addr(vector).ok_or(BootError::IrqIdtInstall)?;
            // SAFETY: BSP after `percpu::init(0)` (run earlier in
            // `try_boot`); interrupts disabled; `vector` is in the
            // reserved external-IRQ range, which never overlaps
            // `#NMI` (2) or `#DF` (8).
            unsafe {
                percpu::install_vector(0, vector, isr_addr)
                    .map_err(|_| BootError::IrqIdtInstall)?;
            }

            routing
                .install(gsi, vector)
                .map_err(|_| BootError::IrqRoutingPublish)?;

            controller_static
                .program_pin(gsi, vector, bsp_lapic_id, /* masked = */ true)
                .map_err(|_| BootError::IrqProgramPin)?;

            if gsi > max_gsi {
                max_gsi = gsi;
            }
        }
    }

    Ok(IrqRouting {
        max_line: max_gsi,
        controller: controller_static as &'static (dyn IrqController + Send + Sync),
    })
}

fn verify_bsp_present(madt: &acpi::Madt<'_>, bsp_lapic_id: u8) -> Result<(), BootError> {
    for entry in madt.entries() {
        if let acpi::MadtEntry::LocalApic { apic_id, flags, .. } = entry {
            // ACPI 6.5 Table 5.40 bit 0 = Processor Enabled.
            if flags & 1 == 0 {
                continue;
            }
            if apic_id == bsp_lapic_id {
                return Ok(());
            }
        }
    }
    Err(BootError::BspLapicMissing)
}

// --- Compile-time invariants ---------------------------------------

// SAFETY-INVARIANT: the production dispatch callback exposes the
// type `syscall_entry::SyscallDispatchFn` expects. The dispatch
// module already pins this at compile time; we re-coerce here at the
// call-site to catch a regression at the `set_dispatch_callback`
// install rather than only in the dispatch module's own tests.
// AGENTS.md §2.4 — encode the contract in the type system.
const _DISPATCH_CALLBACK_INSTALLABLE: syscall_entry::SyscallDispatchFn = production_dispatch;

// SAFETY-INVARIANT: a 16-KiB per-CPU stack is sufficient to hold a
// `[u64; SYSCALL_MAX_ARGS]` frame plus the kernel-side trampoline's
// own activation record many times over. Encode the lower bound
// here so a future shrink of `KERNEL_STACK_BYTES` fails the build
// before reaching QEMU.
const _KERNEL_STACK_FITS_AT_LEAST_ONE_FRAME: () = {
    assert!(KERNEL_STACK_BYTES >= SYSCALL_MAX_ARGS * core::mem::size_of::<u64>() * 16);
};
