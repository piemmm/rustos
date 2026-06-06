//! Bare-metal boot pipeline for the aarch64 (Raspberry Pi 4)
//! `rustos-kernel` binary — `plans/PI.md` Stage P1.
//!
//! [`boot`] is the single entry point. It is called from the binary's
//! `extern "C" fn kernel_main(dtb: u64)` after the arch crate's
//! [`rustos_arch_aarch64::entry`] trampoline (`boot.s` → `entry.rs`)
//! has dropped to EL1, parked the non-boot CPUs, established the boot
//! stack, and zeroed `.bss`.
//!
//! # Scope at Stage P1 (and what is deliberately staged later)
//!
//! P1 stands up the production aarch64 kernel *image*: the boot stub,
//! the Raspberry Pi 4 linker script (`aarch64-rpi4.ld`, load `0x8_0000`),
//! and this binary, with the aarch64 architecture port selected as the
//! kernel's arch (the single `AGENTS.md` §17.1/§17.2 selection point).
//! On the boot CPU it enables FP/SIMD, brings up the console, constructs
//! the [`Aarch64Arch`] handle, records a boot audit line, and parks
//! fail-closed (`AGENTS.md` §2.9 — never silently reset).
//!
//! The discovery-fed hand-off to [`rustos_kernel_core::kernel_main`] is
//! **staged**, not stubbed: it requires a real `BootMemoryMap` and IRQ
//! routing, which only the device-tree discovery and GIC-400 wiring of
//! `plans/PI.md` P2/P3 can honestly supply (fabricating a hardware map
//! would violate `AGENTS.md` §18.5). Those stages add the `-M raspi4b`
//! QEMU verticals that *prove* the runtime path; until then P1's gate is
//! that this image builds and links (`plans/PI.md` P1 "Done when"). The
//! console here uses the port's existing PL011 base, which P2 replaces
//! with the base discovered from the firmware device tree.

use rustos_arch_aarch64::kernel_arch::read_cntfrq;
use rustos_arch_aarch64::{enable_fp_el1, halt_current_cpu, Aarch64Arch};
use rustos_arch_api::SchedulerArch;
use rustos_log::{log, Event, EventId, Field, Level, Sink};

/// The boot CPU's logical id. The boot trampoline parks every other CPU
/// (`MPIDR_EL1` affinity ≠ 0) until the SMP bring-up (`plans/PI.md` P5)
/// starts it, so this binary runs single-CPU on logical CPU 0.
const BOOT_CPU: u32 = 0;

/// Audit event: the aarch64 production kernel reached its Stage-P1 boot
/// init point. Sits in the `kernel/core`-owned `4000..5000` range (per
/// `lib/log`'s subsystem ranges), just below the x86_64 boot pipeline's
/// `4098`/`4099`; the id is part of the audit contract and may not be
/// renumbered (`AGENTS.md` §5.4.4).
const KERNEL_BOOT_AARCH64_REACHED: EventId = EventId(4097);

/// Stable `"true"`/`"false"` audit-field value for a boolean condition.
/// Keeping the boot log to `&'static str` fields means the path takes no
/// allocation and cannot panic (`AGENTS.md` §2.9).
const fn yes_no(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

/// Boot the aarch64 kernel on the boot CPU and park.
///
/// `dtb` is the device-tree pointer the firmware/loader handed the boot
/// CPU (preserved verbatim by `boot.s`). P1 records whether it is
/// present; P2/P3 parse it through `FdtDiscovery` for the console base,
/// the GIC-400 bases, and the memory map.
///
/// `log_sink` is the `&'static` console sink the binary installs (the
/// port's PL011-backed [`rustos_arch_aarch64::SERIAL_SINK`] in
/// production).
///
/// Returns the bottom type: after recording the boot line it parks the
/// CPU forever via [`halt_current_cpu`] (`AGENTS.md` §2.9 — fail closed).
///
/// # SAFETY-INVARIANT
///
/// Called exactly once, on the boot CPU, from the arch trampoline after
/// `boot.s`'s invariants hold (EL1, interrupts masked, stack established,
/// `.bss` zeroed). FP/SIMD is enabled here before any code that the
/// compiler may lower to NEON runs.
pub fn boot(dtb: u64, log_sink: &'static (dyn Sink + Sync)) -> ! {
    // Enable FP/SIMD before the log formatter (which the compiler may
    // lower to NEON) runs. SAFETY: this is the boot CPU, called once,
    // before any FP/SIMD instruction executes (see `enable_fp_el1`).
    unsafe {
        enable_fp_el1();
    }

    // Construct the architecture handle — the single §17.1/§17.2
    // concrete-arch selection point for the kernel image. The counter
    // frequency seeds the handle's monotonic clock.
    let counter_hz = read_cntfrq();
    let arch = Aarch64Arch::new(BOOT_CPU, counter_hz);

    // Sanity-check that the constructed handle reports the boot CPU, and
    // that the generic-timer frequency is usable. A zero frequency would
    // make the monotonic clock unusable, so the line is recorded at
    // `Warn` rather than trusting it silently (`AGENTS.md` §19.1 —
    // record the contract, do not assume it). A single boot proceeds to
    // the park regardless; P4 wires the live timer + scheduler.
    let boot_cpu_ok = arch.current_cpu() == BOOT_CPU;
    let timer_present = counter_hz != 0;
    let level = if boot_cpu_ok && timer_present {
        Level::Info
    } else {
        Level::Warn
    };

    log(
        log_sink,
        &Event {
            level,
            id: KERNEL_BOOT_AARCH64_REACHED,
            message: "rustos-kernel aarch64 (raspberry pi 4): reached stage-p1 boot init point",
            fields: &[
                Field {
                    key: "boot_cpu_ok",
                    value: yes_no(boot_cpu_ok),
                },
                Field {
                    key: "timer_present",
                    value: yes_no(timer_present),
                },
                Field {
                    key: "dtb_present",
                    value: yes_no(dtb != 0),
                },
                Field {
                    key: "next_stage",
                    value: "pi_p2_console_discovery",
                },
            ],
        },
    );

    halt_current_cpu()
}
