//! WIRING Stage W6 QEMU integration test: cross-CPU TLB shootdown on the
//! aarch64 `virt` board.
//!
//! ## What this test asserts
//!
//! The `rustos_arch_api::CrossCpuTlbShootdown` HAL slice requires that a
//! page-table edit on one CPU can be made visible on the others. On
//! aarch64 this needs no IPI: `Aarch64Arch::shootdown_page` issues the
//! *inner-shareable broadcast* `tlbi vaae1is` + `dsb ish`/`isb`, which the
//! hardware propagates to every PE in the inner-shareable domain. This
//! binary proves that path on a real two-core `virt` board:
//!
//! 1. The boot core starts core 1 via `smp::start_secondary` (the PSCI
//!    `CPU_ON` call); core 1 signals `READY` and idles, so the domain
//!    genuinely contains a second PE.
//! 2. The boot core drives `Aarch64Arch::shootdown_page` — the broadcast
//!    invalidation + barriers — and reaches the PASS finisher, proving the
//!    broadcast executes on a real multi-core machine without faulting.
//!
//! Unlike the `riscv64`/`x86_64` verticals there is no software acknowledge to
//! observe: the broadcast and its `dsb ish` completion are the hardware's
//! responsibility, exactly as the local `flush_page` (the same instruction)
//! is already exercised by `memory_isolation_qemu_aarch64`.
//!
//! A regression that fails to start the core or that faults in
//! `shootdown_page` never reaches the PASS write, so the run times out or
//! trips a failure finisher — the documented fail-loud behaviour
//! (`AGENTS.md` §7).
//!
//! ## PSCI conduit
//!
//! As in `ipi_smp_qemu_aarch64`, the QEMU `virt` board's conduit is `hvc`
//! and QEMU's ELF `-kernel` boot hands no DTB pointer, so the test names
//! the board's known conduit directly; the runtime FDT-discovery path is
//! proven by W1's tests, not re-proved here.
//!
//! ## How it differs from a production kernel
//!
//! It links only the `rustos-arch-aarch64` port and supplies its own
//! `kernel_main`. The QEMU-exit shortcut lives in this dedicated bin,
//! never behind a Cargo feature on the arch crate (`AGENTS.md` §5.4.5 —
//! fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, Ordering};

    use rustos_arch_aarch64::kernel_arch::read_cntfrq;
    use rustos_arch_aarch64::{
        fdt, handle_panic_via_serial, qemu_exit, smp, Aarch64Arch, SERIAL_SINK,
    };
    use rustos_arch_api::{CpuId, CrossCpuTlbShootdown};
    use rustos_log::{log, Event, EventId, Level};

    /// Dense id of the boot core (the `virt` board enters on affinity 0).
    const BOOT_CPU: CpuId = 0;

    /// Dense id of the secondary core this test starts.
    const SECONDARY_CPU: CpuId = 1;

    /// `MPIDR_EL1` affinity QEMU assigns core 1 on the `virt` board (the
    /// linear core index).
    const SECONDARY_MPIDR: u64 = SECONDARY_CPU as u64;

    /// PSCI conduit on the QEMU `virt` board (no EL3 → `hvc`); see the
    /// module docs.
    const VIRT_PSCI_METHOD: fdt::PsciMethod = fdt::PsciMethod::Hvc;

    /// A representative page to invalidate. The exact address is
    /// immaterial — a TLB shootdown can only ever *over*-invalidate.
    const SHOOTDOWN_VADDR: u64 = 0x4020_0000;

    /// Stable audit-event ids for the QEMU transcript.
    const TEST_START: EventId = EventId(4264);
    const SECONDARY_UP: EventId = EventId(4265);
    const TEST_PASS: EventId = EventId(4266);

    /// Failure finisher code: the secondary core never came up.
    const FAIL_SECONDARY_START: u16 = 1;
    /// Failure finisher code: `CNTFRQ_EL0` reported a zero frequency.
    const FAIL_ZERO_FREQ: u16 = 3;

    /// Set to `1` by the secondary core once it is up and idling, so the
    /// inner-shareable domain genuinely contains a second PE when the boot
    /// core broadcasts.
    static SECONDARY_READY: AtomicU32 = AtomicU32::new(0);

    /// Entry the secondary core runs (via the `smp.s` trampoline). It only
    /// needs to be a *running* PE in the inner-shareable domain for the
    /// broadcast to reach it, so it signals ready and idles — no GIC or
    /// vector setup is required.
    extern "C" fn secondary_entry(_cpu: CpuId) -> ! {
        SECONDARY_READY.store(1, Ordering::SeqCst);
        loop {
            // SAFETY: `wfi` is a wait-for-interrupt hint with no
            // architectural side effects.
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
            }
        }
    }

    /// Forward to the shared aarch64 panic bridge (parks the core; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn rustos_xtlb_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_aarch64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: TEST_START,
                message: "aarch64 cross-CPU TLB shootdown test: starting secondary core",
                fields: &[],
            },
        );

        let counter_hz = read_cntfrq();
        if counter_hz == 0 {
            qemu_exit::exit_failure(FAIL_ZERO_FREQ);
        }

        let arch =
            Aarch64Arch::with_cpus(BOOT_CPU, counter_hz, &[BOOT_CPU as u64, SECONDARY_MPIDR]);

        if smp::set_secondary_entry(secondary_entry).is_err() {
            qemu_exit::exit_failure(FAIL_SECONDARY_START);
        }
        // SAFETY: called on the boot core after `boot.s` zeroed `.bss`
        // (clearing the secondary stack pool) and after the secondary
        // entry was installed; `SECONDARY_MPIDR` names a real, parked,
        // distinct core.
        if unsafe { smp::start_secondary(VIRT_PSCI_METHOD, SECONDARY_CPU, SECONDARY_MPIDR) }
            .is_err()
        {
            qemu_exit::exit_failure(FAIL_SECONDARY_START);
        }

        // Wait until the secondary core is up, so the broadcast targets a
        // genuinely multi-PE inner-shareable domain.
        while SECONDARY_READY.load(Ordering::SeqCst) == 0 {
            core::hint::spin_loop();
        }
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: SECONDARY_UP,
                message: "aarch64 cross-CPU TLB shootdown test: secondary up, broadcasting",
                fields: &[],
            },
        );

        // Drive the real HAL entry point: the inner-shareable broadcast
        // `tlbi vaae1is` + `dsb ish`/`isb`. Reaching the next line proves
        // the broadcast executed on a real two-core machine without
        // faulting.
        arch.shootdown_page(SHOOTDOWN_VADDR);

        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: TEST_PASS,
                message: "aarch64 cross-CPU TLB shootdown test: broadcast invalidation completed",
                fields: &[],
            },
        );
        qemu_exit::exit_success();
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
