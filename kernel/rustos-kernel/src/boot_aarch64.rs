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

use rustos_arch_aarch64::kernel_arch::{read_cntfrq, timer_frequency_hz};
use rustos_arch_aarch64::{console, enable_fp_el1, fdt, gic, halt_current_cpu, Aarch64Arch};
use rustos_arch_api::SchedulerArch;
use rustos_fdt::Fdt;
use rustos_log::{log, Event, EventId, Field, Level, Sink};
use rustos_util::fmt::format_hex_u64;

use crate::mem_map::{build_memory_map, region_byte_totals};

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

extern "C" {
    /// One byte past the end of the kernel image, including the boot heap,
    /// defined by the board linker script (`aarch64-rpi4.ld` /
    /// `aarch64-virt.ld`). The usable physical-memory region the allocator
    /// receives begins at the next page boundary after this address.
    static __kernel_end: u8;
}

/// Address of the linker-provided `__kernel_end` symbol.
fn kernel_end_addr() -> u64 {
    // `addr_of!` reads the marker's address without forming a reference to
    // the zero-sized, never-dereferenced symbol.
    core::ptr::addr_of!(__kernel_end) as u64
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
    // frequency seeds the handle's monotonic clock and (at P4) the live
    // timer interval; it is the board's device-tree `clock-frequency`
    // override when present, else the `CNTFRQ_EL0` register value
    // (`discovered.timer_hz`).
    let counter_hz = discovered.timer_hz;
    let arch = Aarch64Arch::new(BOOT_CPU, counter_hz);
    // Install the PSCI conduit discovered from the firmware tree (`hvc`
    // at EL2-hosted, `smc` at EL3-hosted — the Pi 4's `armstub8.bin`
    // exposes `smc`), so the SMP bring-up that follows (`plans/PI.md` P5)
    // issues `CPU_ON` through the conduit the board *declares*, never an
    // assumed one (`AGENTS.md` §17.2 — no `cfg(board)` fork). A tree that
    // declares no PSCI node leaves the conduit unset, and bring-up fails
    // closed at the start site rather than guessing (`AGENTS.md` §5.4.5).
    let arch = match discovered.psci_method {
        Some(method) => arch.with_psci_method(method),
        None => arch,
    };

    // Sanity-check that the constructed handle reports the boot CPU, and
    // that the generic-timer frequency is usable. A zero frequency would
    // make the monotonic clock unusable, so the line is recorded at
    // `Warn` rather than trusting it silently (`AGENTS.md` §19.1 —
    // record the contract, do not assume it). A single boot proceeds to
    // the park regardless; P4 wires the live timer + scheduler.
    let boot_cpu_ok = arch.current_cpu() == BOOT_CPU;
    let timer_present = counter_hz != 0;

    // P6c-1: translate the firmware-discovered `/memory` window into the
    // canonical physical-memory map the live allocator hand-off will
    // consume (`plans/PI.md` P6c-2). The map is built and its
    // usable/reserved split recorded here; an absent or malformed window
    // fails closed to a status string rather than a panic
    // (`AGENTS.md` §2.9). Wiring the map into `kernel_core::kernel_main`
    // (which first needs the MMU enabled so the allocator's atomics run on
    // Normal, not Device, memory) is P6c-2.
    let (mem_status, usable_bytes, reserved_bytes) = match discovered.ram_window {
        None => ("no_memory_window", 0, 0),
        Some((base, size)) => match build_memory_map(base, size, kernel_end_addr()) {
            Ok(map) => {
                let (usable, reserved) = region_byte_totals(&map);
                ("built", usable, reserved)
            }
            Err(err) => (err.as_str(), 0, 0),
        },
    };
    let mem_map_built = mem_status == "built";

    let level = if boot_cpu_ok && timer_present && mem_map_built {
        Level::Info
    } else {
        Level::Warn
    };

    // Stack buffers for the allocation-free hex rendering of the discovered
    // byte counts; they must outlive the `fields` slice handed to `log`.
    let mut usable_buf = [0u8; 16];
    let mut reserved_buf = [0u8; 16];
    let usable_hex = format_hex_u64(usable_bytes, &mut usable_buf);
    let reserved_hex = format_hex_u64(reserved_bytes, &mut reserved_buf);

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
                    value: yes_no(discovered.ram_window.is_some()),
                },
                Field {
                    key: "mem_map_built",
                    value: yes_no(mem_map_built),
                },
                Field {
                    key: "mem_map_status",
                    value: mem_status,
                },
                Field {
                    key: "usable_bytes_hex",
                    value: usable_hex,
                },
                Field {
                    key: "reserved_bytes_hex",
                    value: reserved_hex,
                },
                Field {
                    key: "timer_hz_from_tree",
                    value: yes_no(discovered.timer_hz_from_tree),
                },
                Field {
                    key: "psci_conduit_discovered",
                    value: yes_no(discovered.psci_method.is_some()),
                },
                Field {
                    key: "next_stage",
                    value: "pi_p6c2_mmu_kernel_main",
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
    /// The `/memory` window `(base, size)` discovered from the firmware
    /// tree, if any — the RAM extent the `BootMemoryMap` (`plans/PI.md`
    /// P6c-1) reserves the kernel image out of and hands the allocator.
    ram_window: Option<(u64, u64)>,
    /// The generic-timer counter frequency (Hz) to seed the handle and
    /// the P4 live timer with: the `/timer` `clock-frequency` override
    /// when the tree declares one, else the `CNTFRQ_EL0` register value.
    timer_hz: u64,
    /// `true` when `timer_hz` came from the device-tree override rather
    /// than the `CNTFRQ_EL0` register.
    timer_hz_from_tree: bool,
    /// The PSCI conduit (`hvc`/`smc`) the `/psci` node declares, used to
    /// issue `CPU_ON` for SMP bring-up (`plans/PI.md` P5). `None` when the
    /// tree declares no PSCI node, so bring-up fails closed rather than
    /// assuming a conduit.
    psci_method: Option<fdt::PsciMethod>,
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
        ram_window: None,
        // With no usable tree the register is the only counter-rate
        // source; P4's tree override (if any) overwrites this below.
        timer_hz: read_cntfrq(),
        timer_hz_from_tree: false,
        psci_method: None,
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
    out.ram_window = fdt.first_memory_region();
    // P4: prefer the board's `/timer` `clock-frequency` over the
    // `CNTFRQ_EL0` register, so the Pi 4's 54 MHz crystal is honoured
    // when the firmware tree declares it (`AGENTS.md` §17.2 — no
    // `cfg(board)` fork).
    out.timer_hz_from_tree = fdt::timer_clock_frequency(&fdt).is_some_and(|hz| hz != 0);
    out.timer_hz = timer_frequency_hz(&fdt);
    // P5: discover the PSCI conduit (`hvc`/`smc`) the firmware tree
    // declares, so secondary-core bring-up issues `CPU_ON` over the
    // board's conduit rather than an assumed one (`AGENTS.md` §17.2).
    out.psci_method = fdt::psci_method(&fdt);
    out
}
