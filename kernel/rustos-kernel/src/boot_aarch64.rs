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
//! **staged**, not stubbed: bringing up the live allocator + scheduler
//! over the discovered map is `plans/PI.md` P4/P6 (fabricating a hardware
//! map would violate `AGENTS.md` §18.5). Those stages add the QEMU
//! verticals that *prove* the runtime path.
//!
//! # P3: board-discovered interrupt controller + RAM window
//!
//! [`boot`] also points the GICv2 driver at the distributor / CPU-
//! interface bases the firmware tree describes
//! ([`rustos_arch_aarch64::gic::configure_from_fdt`]) — the QEMU `virt`
//! GICv2 or the Pi 4's GIC-400 — and reads the `/memory` window
//! (`first_memory_region`), so neither the interrupt-controller base nor
//! the RAM base is the `virt` assumption any longer. The byte-wise FDT
//! reader makes both walks MMU-off-safe (`plans/PI.md` W17); a missing or
//! malformed tree leaves the fail-safe `virt` defaults in place
//! (`AGENTS.md` §2.9).
//!
//! # P2: board-discovered console
//!
//! Before logging, [`boot`] points the console at the UART the firmware
//! device tree describes ([`rustos_arch_aarch64::console::configure_from_fdt`]).
//! On the QEMU `virt` board this re-confirms the default PL011 base; on a
//! Raspberry Pi it selects the Pi's PL011 (or the BCM2835 AUX mini-UART)
//! at its high-peripheral base, so the boot line prints on real hardware
//! and under `-M raspi3b`. The FDT reader accesses the blob byte-wise, so
//! the walk is safe with the MMU still off (no multi-byte Device-memory
//! load — `plans/PI.md` W17). A missing or malformed tree leaves the
//! `virt` default in place (`AGENTS.md` §2.9 — fail closed).

use rustos_arch_aarch64::kernel_arch::read_cntfrq;
use rustos_arch_aarch64::{console, enable_fp_el1, gic, halt_current_cpu, Aarch64Arch};
use rustos_arch_api::SchedulerArch;
use rustos_fdt::Fdt;
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
/// CPU (preserved verbatim by `boot.s`). P2/P3 parse it for the console
/// base, the GICv2/GIC-400 bases, and the `/memory` window; the live
/// allocator + scheduler hand-off over that map is P4/P6.
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

    // Discover the board from the firmware device tree before any log
    // line is emitted: point the console at the UART, the GICv2 driver at
    // the discovered GICD/GICC bases, and read the `/memory` window (P2 +
    // P3). The FDT reader is byte-wise, so every walk is safe with the MMU
    // still off (`plans/PI.md` W17). A null, unreadable, or incomplete
    // tree leaves the `virt` defaults in place (fail closed,
    // `AGENTS.md` §2.9).
    let discovered = configure_from_dtb(dtb);

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
                    key: "console_discovered",
                    value: yes_no(discovered.console),
                },
                Field {
                    key: "gic_discovered",
                    value: yes_no(discovered.gic),
                },
                Field {
                    key: "ram_discovered",
                    value: yes_no(discovered.ram),
                },
                Field {
                    key: "next_stage",
                    value: "pi_p4_timer_and_scheduler",
                },
            ],
        },
    );

    halt_current_cpu()
}

/// What the boot path resolved from the firmware device tree.
struct Discovered {
    /// A recognised console UART was found and the console base/model set.
    console: bool,
    /// A GICv2-class interrupt controller was found and its GICD/GICC
    /// bases set.
    gic: bool,
    /// A `/memory` region was found (the RAM base/size the P4/P6 allocator
    /// hand-off will consume).
    ram: bool,
}

/// Discover the board from the device tree at `dtb`: point the console
/// and the GICv2 driver at their discovered bases and read the `/memory`
/// window. Each field reports whether that fact was found.
///
/// A null pointer, an unreadable/invalid blob, or a tree missing a fact
/// leaves the corresponding pre-discovery `virt` default untouched (fail
/// closed, `AGENTS.md` §2.9).
fn configure_from_dtb(dtb: u64) -> Discovered {
    let mut out = Discovered {
        console: false,
        gic: false,
        ram: false,
    };
    if dtb == 0 {
        return out;
    }
    // SAFETY: on the boot hand-off `dtb` is the firmware/loader device-tree
    // pointer (`boot.s` preserves x0). `Fdt::from_ptr` validates the magic
    // and bounds the blob by its own `totalsize` before any further read,
    // and every read is a single byte, so the access is valid MMU-off. A
    // bogus pointer fails the magic check and returns `Err` rather than
    // faulting on structured data.
    let Ok(fdt) = (unsafe { Fdt::from_ptr(dtb as *const u8) }) else {
        return out;
    };
    out.console = console::configure_from_fdt(&fdt).is_some();
    out.gic = gic::configure_from_fdt(&fdt).is_some();
    out.ram = fdt.first_memory_region().is_some();
    out
}
