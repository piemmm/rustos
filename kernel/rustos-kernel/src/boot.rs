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

use alloc::sync::Arc;

use rustos_abi::SYSCALL_MAX_ARGS;
use rustos_arch_x86_64::acpi;
use rustos_arch_x86_64::apic::{Lapic, VolatileLapicMmio};
use rustos_arch_x86_64::apic_timer::{self, PolledPit, Rdtsc};
use rustos_arch_x86_64::bootmemory;
use rustos_arch_x86_64::gdt::PerCpuGdt;
use rustos_arch_x86_64::kernel_arch::{halt as arch_halt, X86_64Arch};
use rustos_arch_x86_64::multiboot2::BootInfo as Mb2BootInfo;
use rustos_arch_x86_64::percpu::MAX_CPUS;
use rustos_arch_x86_64::{percpu, preempt, smp, syscall_entry};
use rustos_kernel_core::{kernel_main, BootInfo};
use rustos_kernel_mem::{BootMemoryMap, MemoryRegion, PhysAddr, RegionKind};
use rustos_kernel_sched::SchedulerConfig;
use rustos_kernel_sec::IdentityTableBuilder;
use rustos_log::{Event, EventId, Field, Level, Sink};

use crate::arch_wrapper::BinArch;
use crate::dispatch::fail_closed_dispatch;

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
/// 16 KiB matches the BSP bootstrap stack in `kernel/arch/x86_64::boot.s`
/// and the per-AP stacks in `scheduler_stress_qemu`. The stack hosts
/// the kernel side of a `syscall` transition (frame layout in
/// `syscall_entry::syscall_entry_stub`); 16 KiB is comfortably above
/// the worst-case kernel-side stack footprint for the (c7-bin)
/// pipeline.
const KERNEL_STACK_BYTES: usize = 16 * 1024;

/// 16-byte-aligned kernel-stack slot. Matches the System V AMD64
/// ABI's 16-byte stack-alignment requirement at function entry.
#[repr(C, align(16))]
struct KernelStack([u8; KERNEL_STACK_BYTES]);

impl KernelStack {
    const ZERO: Self = Self([0; KERNEL_STACK_BYTES]);
}

/// Per-CPU kernel stack pool. The (c7-bin) bring-up only initialises
/// the BSP (`cpu_index = 0`); slots `1..MAX_CPUS` exist so the
/// Stage 2.7 AP-bring-up commit can populate them without re-laying-
/// out this static.
///
/// AGENTS.md §2 — the only `static mut` in the bin crate, justified
/// in `README.md` as the per-CPU bootstrap-stack arena. Access is
/// exclusively through [`kernel_stack_top`], which derives a
/// disjoint pointer per `cpu_index`.
static mut KERNEL_STACKS: [KernelStack; MAX_CPUS] = {
    const Z: KernelStack = KernelStack::ZERO;
    [Z; MAX_CPUS]
};

/// One byte past the top of `KERNEL_STACKS[cpu_index]`.
///
/// `cpu_index < MAX_CPUS` is the caller's responsibility; [`boot`]
/// satisfies that statically (it only calls with `0`).
fn kernel_stack_top(cpu_index: usize) -> u64 {
    debug_assert!(cpu_index < MAX_CPUS);
    // SAFETY: `cpu_index < MAX_CPUS` per the debug assert above
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
    /// `percpu::init` rejected the BSP.
    PercpuInit,
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
            Self::PercpuInit => "percpu_init_failed",
            Self::TimerCalibration => "timer_calibration_failed",
            Self::PreemptInit => "preempt_init_failed",
            Self::SyscallInit => "syscall_init_failed",
            Self::ArchInit => "arch_init_failed",
            Self::BootInfoInvalid => "bootinfo_invalid",
        }
    }
}

/// Audit event the boot pipeline emits on failure. Kept separate from
/// the `kernel/core` audit catalogue because the failure happens
/// *before* `kernel_core::kernel_main` is ever entered (and therefore
/// before its phase events have any meaning).
///
/// EventId `4099` sits in the `4000..5000` range owned by `kernel/core`
/// (per `lib/log`'s subsystem ranges) but at the top of the range so
/// it cannot collide with any phase-numbered event. The id is part of
/// the audit contract with external consumers and may not be renumbered
/// (`AGENTS.md` §5.4.4).
const KERNEL_BOOT_INIT_FAILED: EventId = EventId(4099);

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
    // SAFETY: This is the BSP, called exactly once. The boot
    // trampoline (`boot.s`) leaves `IF=0` so interrupts remain
    // disabled, satisfying `percpu::init`'s SAFETY contract.
    unsafe { percpu::init(0).map_err(|_| BootError::PercpuInit)? };

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

    let memory_map = build_memory_map(&mb2)?;
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
    //    Stage 2.7 AP bring-up will fill in the remaining slots; for
    //    (c7-bin) the kernel is single-CPU and `scheduler_config.cpus`
    //    matches that.
    let mut cpu_to_lapic: [Option<u8>; MAX_CPUS] = [None; MAX_CPUS];
    cpu_to_lapic[0] = Some(bsp_lapic_id);

    let arch = X86_64Arch::new(0, bsp_lapic_id, cpu_to_lapic).map_err(|_| BootError::ArchInit)?;

    // 7. Install the fail-closed syscall-dispatch callback **before**
    //    `init_local_syscalls` enables `syscall` on any CPU. The
    //    ordering matters per `syscall_entry` rustdoc — the trampoline
    //    fail-closes if it fires with no callback installed.
    syscall_entry::set_dispatch_callback(fail_closed_dispatch);

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

    // 11. Assemble the `BootInfo` and hand off to `kernel_core`.
    //
    // Build the `Arc<BinArch>` ahead of the `BootInfo::new` call so we
    // can publish the pointer into `panic_ctx::PANIC_ARCH_PTR` for the
    // panic-handler bridge. The `Arc` is kept alive by `BootInfo`'s
    // `arch` field (and re-cloned into `kernel_core`'s `KernelState`),
    // so the published pointer remains valid for the lifetime of the
    // running kernel.
    let arch_arc: Arc<BinArch> = Arc::new(BinArch::new(arch, calibration));
    // SAFETY: `arch_arc` is moved into `BootInfo` immediately below
    // (which `kernel_main` consumes and stores). `Arc::as_ptr` returns
    // a stable pointer for the lifetime of any clone of the `Arc`.
    unsafe {
        crate::panic_ctx::publish_arch(Arc::as_ptr(&arch_arc));
    }
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
    );
    boot_info
        .validate()
        .map_err(|_| BootError::BootInfoInvalid)?;

    // The caller forwards to `kernel_main`, which returns `!` and
    // never re-enters this function.
    Ok(boot_info)
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

    Ok(map)
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

// SAFETY-INVARIANT: the fail-closed dispatch callback exposes the
// type `syscall_entry::SyscallDispatchFn` expects. The dispatch
// module already pins this at compile time; we re-coerce here at the
// call-site to catch a regression at the `set_dispatch_callback`
// install rather than only in the dispatch module's own tests.
// AGENTS.md §2.4 — encode the contract in the type system.
const _DISPATCH_CALLBACK_INSTALLABLE: syscall_entry::SyscallDispatchFn = fail_closed_dispatch;

// SAFETY-INVARIANT: a 16-KiB per-CPU stack is sufficient to hold a
// `[u64; SYSCALL_MAX_ARGS]` frame plus the kernel-side trampoline's
// own activation record many times over. Encode the lower bound
// here so a future shrink of `KERNEL_STACK_BYTES` fails the build
// before reaching QEMU.
const _KERNEL_STACK_FITS_AT_LEAST_ONE_FRAME: () = {
    assert!(KERNEL_STACK_BYTES >= SYSCALL_MAX_ARGS * core::mem::size_of::<u64>() * 16);
};
