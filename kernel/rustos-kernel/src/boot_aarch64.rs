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

use alloc::sync::Arc;

use rustos_arch_aarch64::kernel_arch::{read_cntfrq, timer_frequency_hz};
use rustos_arch_aarch64::paging::{
    configure_device_gigapages, configure_ram_gigapages, identity_device_mask, identity_ram_mask,
    ram_gigapages, AddressSpace, PageTablePool,
};
use rustos_arch_aarch64::{
    console, enable_fp_el1, exceptions, fdt, gic, halt_current_cpu, platform, syscall_entry,
    uart_init, video, Aarch64Arch, Aarch64ArchStorage,
};
use rustos_arch_api::SchedulerArch;
use rustos_fdt::Fdt;
use rustos_kernel_core::{kernel_main, BootInfo};
use rustos_kernel_sched_api::SchedulerConfig;
use rustos_kernel_sec::IdentityTableBuilder;
use rustos_log::{log, Event, EventId, Field, Level, Sink};
use rustos_util::fmt::format_hex_u64;

use crate::arch_wrapper_aarch64::{
    Aarch64BinArch, INPUT_FOCUS, UART_ONLY_CONSOLES, VIDEO_AND_UART_CONSOLES,
};
use crate::dispatch_aarch64::{production_dispatch, DISPATCH_SLOT};
use crate::mem_map::{build_memory_map, region_byte_totals};

/// Number of 1 GiB identity gigapages the boot address space maps.
///
/// 512 covers the whole low canonical VA range (`[0, 512 GiB)`), so the
/// kernel image, stack, the firmware DTB, and the board MMIO window are
/// all reachable whatever their physical addresses — the QEMU `virt`
/// board (RAM at `0x4000_0000`, GIC/PL011 in the first GiB) and the
/// Raspberry Pi 4 (RAM at `0`, MMIO in gigapage 3) alike, with no
/// `cfg(board)` fork (`AGENTS.md` §17.2).
/// [`AddressSpace::new_identity_gigapages`] maps the gigapages holding
/// the *discovered* console/GIC MMIO Device and the rest — the kernel's
/// own image included — Normal, per the [`configure_device_gigapages`]
/// mask [`boot`] derives before enabling the MMU.
const IDENTITY_GIGABYTES: usize = 512;

/// Boot-time page-table frame source for the stage-1 identity map.
///
/// A single root L1 table holds all 512 gigapage block descriptors, so
/// the pool only ever hands out one frame here. It lives in `.bss` for
/// the lifetime of the kernel image, so `TTBR0_EL1` keeps pointing at a
/// valid table after [`enable_mmu_and_vectors`] returns even though the
/// transient [`AddressSpace`] handle is dropped (`AGENTS.md` §2.1 — the
/// pool is monotonic and never freed). The real per-process page tables
/// are built over the `kernel/mem` frame allocator at a later stage.
static BOOT_PAGE_TABLES: PageTablePool = PageTablePool::new();

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
    /// First byte of the kernel image — the board load address — defined
    /// by the board linker script (`aarch64-rpi4.ld` / `aarch64-virt.ld`).
    /// With [`__kernel_end`] it brackets the image extent whose identity
    /// gigapages must stay Normal (executable) whatever the discovered
    /// MMIO layout says ([`identity_device_mask`]).
    static __kernel_start: u8;
    /// One byte past the end of the kernel image, including the boot heap,
    /// defined by the board linker script (`aarch64-rpi4.ld` /
    /// `aarch64-virt.ld`). The usable physical-memory region the allocator
    /// receives begins at the next page boundary after this address.
    static __kernel_end: u8;
}

/// Address of the linker-provided `__kernel_start` symbol.
fn kernel_start_addr() -> u64 {
    // `addr_of!` reads the marker's address without forming a reference to
    // the zero-sized, never-dereferenced symbol.
    core::ptr::addr_of!(__kernel_start) as u64
}

/// Address of the linker-provided `__kernel_end` symbol.
fn kernel_end_addr() -> u64 {
    // `addr_of!` reads the marker's address without forming a reference to
    // the zero-sized, never-dereferenced symbol.
    core::ptr::addr_of!(__kernel_end) as u64
}

/// Enable the stage-1 identity MMU and install the EL1 exception
/// vectors on the boot CPU, returning the live boot [`AddressSpace`].
///
/// Returns `None` (leaving the MMU off) when the boot page-table pool
/// cannot satisfy the identity map — a fail-closed signal the caller
/// logs and parks on rather than running `kernel_main` over un-cacheable
/// Device memory (`AGENTS.md` §2.9).
///
/// The handle is returned, not dropped, so the caller can re-express the
/// kthread-stack guard arena at 4 KiB granularity over the *active*
/// tables once the RAM window has been discovered
/// ([`AddressSpace::prepare_guard_arena`], `plans/PI.md` stage G2). The
/// returned space still owns `TTBR0_EL1`'s root table
/// ([`BOOT_PAGE_TABLES`]), which lives for the kernel's lifetime.
fn enable_mmu_and_vectors() -> Option<AddressSpace> {
    let space = AddressSpace::new_identity_gigapages(&BOOT_PAGE_TABLES, IDENTITY_GIGABYTES)?;
    // The tables were just written with the data cache off; sweep them
    // to the point of coherency so the walker's first *cacheable* reads
    // cannot hit stale firmware-era lines on real silicon.
    BOOT_PAGE_TABLES.clean_invalidate_to_poc();
    // SAFETY: `new_identity_gigapages` identity-maps every gigapage in
    // the configured Device and RAM masks — the caller installed both
    // before this runs, and the RAM mask is built over the kernel
    // image's own extent, the firmware DTB, and the scan-out surface
    // (`identity_ram_mask`) — so the currently-executing `pc`, the boot
    // stack, the firmware DTB, and the board MMIO window all keep their
    // physical addresses: enabling the MMU does not move the ground
    // under the running code, exactly as `AddressSpace::switch`'s
    // contract requires (unbacked gigapages are deliberately left
    // invalid so speculation cannot wander into them). `init_vectors`
    // then installs the EL1 vector base so a fault during the remaining
    // bring-up is taken to a handler rather than locking up silently.
    // Both run once, here, on the boot CPU with interrupts masked.
    unsafe {
        space.switch();
    }
    // SAFETY: covered by the block comment above — vectors are installed
    // once, on the boot CPU, immediately after the switch.
    unsafe {
        exceptions::init_vectors();
    }
    Some(space)
}

/// Boot the aarch64 kernel on the boot CPU and hand off to
/// [`rustos_kernel_core::kernel_main`].
///
/// `dtb` is the device-tree pointer the firmware/loader handed the boot
/// CPU (preserved verbatim by `boot.s`); it is parsed for the console
/// base, the GICv2/GIC-400 bases, the `/memory` window, the generic-timer
/// rate, and the PSCI conduit.
///
/// `log_sink` / `audit_sink` are the `&'static` sinks installed in the
/// [`BootInfo`]: in production both are the port's PL011-backed
/// [`rustos_arch_aarch64::SERIAL_SINK`]; the boot-completed QEMU vertical
/// substitutes an audit sink that exits QEMU on `AuditEvent::BootCompleted`.
///
/// Returns the bottom type. On success it enters
/// [`rustos_kernel_core::kernel_main`], which drives the init phases and
/// itself never returns. If the handover cannot be assembled (no usable
/// `/memory` window, the MMU could not be enabled, an unusable timer, or
/// `BootInfo::validate` rejects the hand-off) it records the boot line
/// and parks the CPU forever via [`halt_current_cpu`] (`AGENTS.md` §2.9 —
/// fail closed).
///
/// # SAFETY-INVARIANT
///
/// Called exactly once, on the boot CPU, from the arch trampoline after
/// `boot.s`'s invariants hold (EL1, interrupts masked, stack established,
/// `.bss` zeroed). FP/SIMD is enabled here before any code that the
/// compiler may lower to NEON runs.
pub fn boot(
    dtb: u64,
    log_sink: &'static (dyn Sink + Sync),
    audit_sink: &'static (dyn Sink + Sync),
) -> ! {
    // Enable FP/SIMD before the log formatter (which the compiler may
    // lower to NEON) runs. SAFETY: this is the boot CPU, called once,
    // before any FP/SIMD instruction executes (see `enable_fp_el1`).
    unsafe {
        enable_fp_el1();
    }

    // P2 + P3, *before* the MMU comes on: point the console at the UART
    // and the GICv2 driver at the GICD/GICC bases the firmware tree
    // describes. Both walks early-return at their matched node
    // (`rustos_arch_aarch64::fdt::scan_translated`), so they are safe
    // MMU-off (`plans/PI.md` watch-out); a null, unreadable, or
    // incomplete tree leaves the `virt` defaults in place (fail closed,
    // `AGENTS.md` §2.9). They must run *before* the identity map is
    // built, because the discovered bases decide which gigapages the map
    // types Device: on the Pi 4 the PL011/GIC-400 live in gigapage 3
    // while the kernel image at `0x8_0000` must keep gigapage 0 Normal —
    // executable — or the instruction fetch after `switch` faults with
    // the vectors not yet installed (the `virt`-only "GiB 0 Device"
    // assumption this replaces).
    let early = configure_mmio_from_dtb(dtb);
    let (console_base, _) = console::current();
    let (gicd_base, gicc_base) = gic::current();
    // The mailbox doorbell the video console rang is an MMIO window the
    // identity map must type Device like the UART and GIC (on the Pi 4
    // they all share gigapage 3, but the mask is derived from facts,
    // never assumed). With no video console the console base stands in
    // as a harmless duplicate input.
    let video_doorbell = early.video.map_or(console_base as u64, |v| v.doorbell_base);
    // The BCM2711 PCIe root complex's controller register block and its
    // outbound MMIO window (where the enumerated VL805 BAR lives) are MMIO
    // the in-kernel USB-keyboard service maps at their identity address
    // (`crate::keyboard_service`), so their gigapages must be typed Device
    // like the UART/GIC. Derived from the discovered `brcm,bcm2711-pcie`
    // node, never assumed; with no such node (the QEMU `virt` shape) the
    // console base stands in as a harmless duplicate input (§18.4 / §18.5).
    let (pcie_regs, pcie_outbound) = early
        .pcie
        .map_or((console_base as u64, console_base as u64), |p| {
            (p.regs_phys, p.outbound_cpu_base)
        });
    let device_mask = identity_device_mask(
        &[
            console_base as u64,
            gicd_base as u64,
            gicc_base as u64,
            video_doorbell,
            pcie_regs,
            pcie_outbound,
        ],
        kernel_start_addr(),
        kernel_end_addr(),
    );
    configure_device_gigapages(device_mask);
    // Hand the discovered PCIe windows to the PID 1 spawn seam, which
    // starts the USB-keyboard service kthread once the scheduler is up
    // (`plans/PI.md` P10). Recorded here, pre-MMU, while the discovery is
    // in hand; a board with no bridge records nothing and the service is
    // never started (§18.4).
    if let Some(pcie) = early.pcie {
        crate::keyboard_service::record_discovery(pcie);
    }
    // RAM gigapage mask from the facts in hand pre-MMU: the kernel
    // image's own extent, the firmware DTB blob, and the firmware
    // scan-out surface. Every other non-Device gigapage stays *invalid*
    // in the identity map — on real silicon a Normal write-back
    // executable mapping of unbacked address space invites the core's
    // speculative fetches into windows nothing answers, which wedged
    // the metal Pi 4B at the instant translation enabled while QEMU
    // (which answers every address) stayed green. The post-MMU
    // `/memory` discovery widens this mask below.
    let (fb_base, fb_len) = early.video.map_or((0, 0), |v| (v.fb_base, v.fb_len_bytes));
    configure_ram_gigapages(identity_ram_mask(&[
        (
            kernel_start_addr(),
            kernel_end_addr().saturating_sub(kernel_start_addr()),
        ),
        (dtb, early.dtb_len),
        (fb_base, fb_len),
    ]));

    // P6c-2: enable the stage-1 identity MMU and EL1 vectors before any
    // further work. The `kernel_core` allocator and scheduler use atomic
    // read-modify-write instructions, which are UNPREDICTABLE on the
    // MMU-off Device-typed memory the boot CPU may run on; the identity
    // map makes RAM Normal/cacheable so they behave (`plans/PI.md`
    // P6c-2). It also makes the full-tree FDT walks below
    // (`first_memory_region`) safe, which fault MMU-off under release
    // optimisation (`plans/PI.md` watch-out).
    let mut boot_space = enable_mmu_and_vectors();
    let mmu_on = boot_space.is_some();

    // Discover the rest of the board from the firmware device tree: the
    // `/memory` window, the timer rate, and the PSCI conduit (P3 + P4 +
    // P5) — full-tree walks that need the MMU on. A null, unreadable, or
    // incomplete tree leaves the `virt` defaults in place (fail closed,
    // `AGENTS.md` §2.9).
    let discovered = configure_from_dtb(dtb);
    // Widen the RAM gigapage mask with the discovered `/memory` window
    // — a walk that is only safe post-MMU — so later-built process
    // spaces map the whole window, and install the widened gigapages
    // into the *live* boot space (an invalid→valid L1 update, no TLB
    // shootdown needed) before the allocator touches the window.
    if let Some((ram_base, ram_size)) = discovered.ram_window {
        let window_mask = identity_ram_mask(&[(ram_base, ram_size)]);
        let mut merged = ram_gigapages();
        for (word, add) in merged.iter_mut().zip(window_mask) {
            *word |= add;
        }
        configure_ram_gigapages(merged);
        if let (Some(space), Some(last_byte)) = (
            boot_space.as_mut(),
            (ram_size > 0).then(|| ram_base.saturating_add(ram_size - 1)),
        ) {
            let mut gigapage = ram_base >> 30;
            while gigapage <= (last_byte >> 30) {
                space.ensure_identity_gigapage(gigapage << 30);
                gigapage += 1;
            }
        }
    }

    // Construct the architecture handle — the single §17.1/§17.2
    // concrete-arch selection point for the kernel image. The counter
    // frequency seeds the handle's monotonic clock and (at P4) the live
    // timer interval; it is the board's device-tree `clock-frequency`
    // override when present, else the `CNTFRQ_EL0` register value
    // (`discovered.timer_hz`).
    let counter_hz = discovered.timer_hz;
    // Per-CPU bookkeeping backing (`AGENTS.md` §24.1): the boot/timer
    // slice brings up only the boot core, so one slot suffices today;
    // sizing this from the discovered CPU count is the SMP-bring-up
    // increment that also resizes the `smp.s` secondary-stack pool.
    static ARCH_STORAGE: Aarch64ArchStorage<1> = Aarch64ArchStorage::new();
    let arch = Aarch64Arch::new(&ARCH_STORAGE, BOOT_CPU, counter_hz);
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
    // make the monotonic clock unusable.
    let boot_cpu_ok = arch.current_cpu() == BOOT_CPU;
    let timer_present = counter_hz != 0;

    // P6c-1: translate the firmware-discovered `/memory` window into the
    // canonical physical-memory map `kernel_core::kernel_main` consumes.
    // An absent or malformed window fails closed to a status string
    // rather than a panic (`AGENTS.md` §2.9); the map is retained (not
    // just measured) so it can be moved into the `BootInfo` hand-off.
    let layout_result: Result<crate::mem_map::MemoryLayout, &'static str> =
        match discovered.ram_window {
            None => Err("no_memory_window"),
            Some((base, size)) => {
                build_memory_map(base, size, kernel_end_addr()).map_err(|err| err.as_str())
            }
        };
    let (mem_status, usable_bytes, reserved_bytes) = match &layout_result {
        Ok(layout) => {
            let (usable, reserved) = region_byte_totals(&layout.map);
            ("built", usable, reserved)
        }
        Err(status) => (*status, 0, 0),
    };
    let mem_map_built = layout_result.is_ok();

    // G2: re-express the reserved kthread-stack guard arena at 4 KiB
    // granularity over the *active* boot tables, so a guard page in it can
    // later be unmapped without shattering the 2 MiB block the CPU runs on
    // (`plans/PI.md` stage G2 → G3). The split only *adds* table levels
    // reproducing the existing translation, so it is safe against the live
    // regime and needs no TLB maintenance. A window too small to carve an
    // arena, or a pool that cannot supply the replacement tables, leaves
    // the guard in its software-canary form — fail closed, never fatal to
    // boot (`AGENTS.md` §2.9 / `plans/PI.md` G2 watch-out).
    let arena_prepared = match (boot_space.as_mut(), &layout_result) {
        (Some(space), Ok(layout)) => layout
            .arena
            .is_some_and(|arena| space.prepare_guard_arena(arena.base, arena.len).is_ok()),
        _ => false,
    };

    // G3b-2: publish the reserved guard arena to the kthread-stack
    // allocator so the PID 1 spawn seam can draw `init`'s kernel stack out
    // of it and unmap that stack's guard page in `init`'s own page-table
    // root — turning a stack overrun into a synchronous fault rather than a
    // poison-canary detection (`plans/PI.md` G3b-2). Installed from the
    // carved arena regardless of `arena_prepared` (which only re-expresses
    // the arena over the *boot* tables): `init` builds its own root and
    // splits the arena there independently; it needs only that the arena's
    // frames are `Reserved` (the memory-map builder guarantees this). When
    // no arena was carved the install is skipped and the seam falls back to
    // a software-canary `BoxStack` (fail closed, `AGENTS.md` §2.17).
    if let Ok(layout) = &layout_result {
        if let Some(arena) = layout.arena {
            crate::stack_arena::KTHREAD_STACK_ARENA.install(
                arena.base,
                arena.len,
                &crate::stack_arena::IdentityBlockStore,
            );
        }
    }

    let ready = boot_cpu_ok && timer_present && mem_map_built && mmu_on;

    let level = if ready { Level::Info } else { Level::Warn };

    // Stack buffers for the allocation-free hex rendering of the discovered
    // byte counts; they must outlive the `fields` slice handed to `log`.
    let mut usable_buf = [0u8; 16];
    let mut reserved_buf = [0u8; 16];
    let usable_hex = format_hex_u64(usable_bytes, &mut usable_buf);
    let reserved_hex = format_hex_u64(reserved_bytes, &mut reserved_buf);
    // Low word of the Device gigapage mask (gigapages 0..64 — both
    // supported boards keep all their MMIO below 64 GiB), recorded so a
    // metal bring-up log shows which gigapages the identity map typed
    // Device (`virt`: 0x1; Pi 4: 0x8).
    let mut device_mask_buf = [0u8; 16];
    let device_mask_hex = format_hex_u64(device_mask[0], &mut device_mask_buf);

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
                    value: yes_no(early.console),
                },
                Field {
                    key: "gic_discovered",
                    value: yes_no(early.gic),
                },
                Field {
                    key: "video_console",
                    value: yes_no(early.video.is_some()),
                },
                Field {
                    key: "device_gigapages_hex",
                    value: device_mask_hex,
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
                    key: "mmu_enabled",
                    value: yes_no(mmu_on),
                },
                Field {
                    key: "guard_arena_prepared",
                    value: yes_no(arena_prepared),
                },
                Field {
                    key: "next_stage",
                    value: "pi_p6c3_spawn_init_el0",
                },
            ],
        },
    );

    // Hand off to the architecture-neutral kernel core when the handover
    // is sound; otherwise park fail-closed (`AGENTS.md` §2.9). The map is
    // moved into `BootInfo` here, so it is built exactly once.
    if ready {
        if let Ok(layout) = layout_result {
            enter_kernel_core(arch, layout.map, log_sink, audit_sink)
        }
    }

    halt_current_cpu()
}

/// Assemble the validated [`BootInfo`] hand-off and enter
/// [`rustos_kernel_core::kernel_main`].
///
/// Installs the production `svc` dispatch callback before user space can
/// be entered (the `kernel_core` `Syscall` phase publishes the resident
/// hook into [`DISPATCH_SLOT`]), wraps the validated [`Aarch64Arch`] in
/// the local [`Aarch64BinArch`] `KernelArch`, and installs the discovered
/// console list: with the framebuffer boot console active the video
/// console is index 0 and the UART an independent second console (one
/// login session each, `plans/PI.md` P11); otherwise the UART is the
/// only console. A hand-off that `BootInfo::validate` rejects parks
/// fail-closed rather than entering the core (`AGENTS.md` §2.9 /
/// §5.4.5).
fn enter_kernel_core(
    arch: Aarch64Arch,
    memory_map: rustos_kernel_mem::BootMemoryMap,
    log_sink: &'static (dyn Sink + Sync),
    audit_sink: &'static (dyn Sink + Sync),
) -> ! {
    // The arch port's `svc` trampoline fail-closes if it fires before a
    // callback is installed, so install it before any user thread runs.
    // No user space exists yet at P6c-2; pinning it here keeps the
    // ordering identical to the x86_64 boot path (`AGENTS.md` §5.4.5).
    syscall_entry::set_dispatch_callback(production_dispatch);

    let arch = Arc::new(Aarch64BinArch::new(arch));
    let boot_info = BootInfo::new(
        BOOT_CPU,
        1,
        "",
        memory_map,
        IdentityTableBuilder::new(),
        SchedulerConfig::defaults_for(1),
        arch,
        log_sink,
        audit_sink,
        Level::Info,
        &DISPATCH_SLOT,
    )
    // Install the discovered console list (`plans/PI.md` P11): when the
    // P7b framebuffer boot console came up, the display (with its
    // keyboard input seam) is the primary console and the UART is an
    // independent second console with its own login session; with no
    // display, the discovered UART is the only console. Each entry is
    // a `stream_write`/`stream_read` backing pair (`AGENTS.md` §20).
    .with_consoles(if video::is_active() {
        &VIDEO_AND_UART_CONSOLES
    } else {
        &UART_ONLY_CONSOLES
    })
    // Install the kernel input-focus arbiter (`plans/PI.md` P11 — input
    // follows the surface owner): its text sink is the video console's
    // keyboard queue, so an injected key press reaches the video login by
    // default, and the window manager's `display_acquire` later routes whole
    // records to the desktop keyboard channel instead (`AGENTS.md` §10 / §20).
    .with_input_focus(&INPUT_FOCUS)
    // Install the PID 1 spawn seam (`plans/PI.md` P6c-3): once every init
    // phase has succeeded and `kernel_main` emits `BootCompleted`, the core
    // invokes it to build `init`'s EL0 image and drop into user mode.
    .with_init(&crate::init_spawn::AARCH64_INIT_SPAWN)
    // Install the runtime `spawn` producer + embedded-program registry
    // (`plans/SPAWN.md` SP3b): the `spawn` syscall resolves a path against
    // the registry and drives the producer to build a fresh, isolated child
    // address space, so PID 1 `init` can launch the user's session.
    .with_spawn(
        &crate::spawn_producer::AARCH64_PROGRAM_REGISTRY,
        &crate::spawn_producer::AARCH64_PROCESS_SPAWN,
    );
    if boot_info.validate().is_err() {
        halt_current_cpu()
    }
    kernel_main(boot_info)
}

/// What the pre-MMU boot phase resolved from the firmware device tree:
/// the MMIO facts the identity map's Device gigapage mask is derived
/// from ([`identity_device_mask`]).
struct EarlyDiscovered {
    /// A recognised console UART was found and the console base/model set.
    console: bool,
    /// A GICv2-class interrupt controller was found and its GICD/GICC
    /// bases set.
    gic: bool,
    /// The framebuffer boot console came up: a firmware mailbox was
    /// found, a display is attached, and the scan-out surface was
    /// allocated — console output now defaults to the screen, with the
    /// UART as the fallback (`plans/PI.md` P7b, `AGENTS.md` §10).
    video: Option<video::DiscoveredVideo>,
    /// Total byte length of the firmware device-tree blob (`totalsize`),
    /// `0` when no readable tree was found — the blob's RAM extent must
    /// stay in the identity map for the post-MMU walks.
    dtb_len: u64,
    /// The BCM2711 PCIe root-complex windows, when the tree describes a
    /// `brcm,bcm2711-pcie` bridge (`plans/PI.md` P10): the controller
    /// register block and outbound MMIO window must be folded into the
    /// identity Device mask, and the bring-up consumes all three windows.
    /// `None` on a board with no bridge (the QEMU `virt` shape, §18.4).
    pcie: Option<platform::PcieDiscovery>,
}

/// Point the console and the GICv2 driver at the bases the firmware tree
/// describes and bring up the framebuffer boot console, before the MMU
/// is enabled.
///
/// All three discoveries are early-returning, `ranges`-aware walks
/// ([`console::configure_from_fdt`] / [`gic::configure_from_fdt`] /
/// [`video::configure_from_fdt`] over
/// [`rustos_arch_aarch64::fdt::scan_translated`]), so they are safe with
/// the MMU off — and the video bring-up *requires* this phase: with the
/// data caches off the CPU↔firmware mailbox exchange is coherent without
/// cache maintenance, and its state cell needs the single-threaded boot
/// CPU. A null pointer, an unreadable/invalid blob, or a tree missing a
/// fact leaves the corresponding pre-discovery default untouched — for
/// video, the UART console (fail closed, `AGENTS.md` §2.9).
fn configure_mmio_from_dtb(dtb: u64) -> EarlyDiscovered {
    let mut out = EarlyDiscovered {
        console: false,
        gic: false,
        video: None,
        dtb_len: 0,
        pcie: None,
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
    out.dtb_len = fdt.total_size() as u64;
    out.console = console::configure_from_fdt(&fdt).is_some();
    // Bring the discovered console's line up before the first log byte:
    // on real Pi 4 silicon UART0 stays silent until GPIO 14/15 are muxed
    // to the PL011 and its line registers are programmed — QEMU's
    // powered-up PL011 masks the omission (`uart_init`).
    uart_init::init_from_fdt(&fdt);
    out.gic = gic::configure_from_fdt(&fdt).is_some();
    out.video = video::configure_from_fdt(&fdt);
    // Discover the BCM2711 PCIe bridge's windows for the in-kernel
    // USB-keyboard service (`plans/PI.md` P10). The early-returning
    // `scan_translated` walk is MMU-off-safe (it reads only the matched
    // node's own properties), exactly like the console/GIC/video walks
    // above; the QEMU `virt` tree carries no such node, so this is `None`
    // there and the keyboard service is never started (§18.4 / §2.9).
    out.pcie = platform::pcie_bringup(&fdt);
    out
}

/// What the post-MMU boot phase resolved from the firmware device tree.
struct Discovered {
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

/// Discover the board's post-MMU facts from the device tree at `dtb`:
/// the `/memory` window, the timer rate, and the PSCI conduit. (The
/// console and GIC bases are configured MMU-off by
/// [`configure_mmio_from_dtb`], before the identity map is built.)
///
/// A null pointer, an unreadable/invalid blob, or a tree missing a fact
/// leaves the corresponding pre-discovery `virt` default untouched (fail
/// closed, `AGENTS.md` §2.9).
fn configure_from_dtb(dtb: u64) -> Discovered {
    let mut out = Discovered {
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
    // and every read is a single byte. The full-tree walks below run with
    // the MMU on (the caller enables it first). A bogus pointer fails the
    // magic check and returns `Err` rather than faulting on structured
    // data.
    let Ok(fdt) = (unsafe { Fdt::from_ptr(dtb as *const u8) }) else {
        return out;
    };
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
