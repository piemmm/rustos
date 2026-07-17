//! Stage 3c QEMU integration test: multi-hart SMP bring-up and IPI
//! delivery on the riscv64 `virt` board.
//!
//! ## What this test asserts
//!
//! The Stage-3 riscv64 SMP follow-up requires that the boot hart can
//! (1) start a secondary hart and (2) deliver a directed inter-processor
//! interrupt to it. This binary exercises both, end to end, on a
//! two-hart `virt` board:
//!
//! 1. The boot hart (hart 0) installs the shared IPI callback
//!    (`preempt::set_ipi_callback`) and the secondary-hart entry
//!    (`smp::set_secondary_entry`).
//! 2. It starts hart 1 via `smp::start_secondary` (the SBI HSM
//!    `hart_start` call), which runs the `smp.s` trampoline → the
//!    installed entry on hart 1.
//! 3. Hart 1 installs the S-mode trap vector (`trap::init_traps`),
//!    enables supervisor software interrupts (`preempt::enable_ipi`),
//!    publishes a `READY` flag, then idles on `wfi`.
//! 4. The boot hart waits for `READY`, then sends an IPI to logical CPU
//!    1 through `RiscvArch::send_ipi` (the SBI IPI extension), which
//!    raises `sip.SSIP` on hart 1.
//! 5. Hart 1 takes the supervisor-software-interrupt trap, which runs
//!    `preempt::on_software_interrupt` → the IPI callback, recording the
//!    hart the callback fired on. The boot hart waits for the callback
//!    to fire on hart 1, then writes the `SiFive` Test PASS finisher.
//!
//! A regression that fails to start the secondary hart or to deliver the
//! IPI never reaches the PASS write, so the run times out and the
//! harness reports `Outcome::Timeout` — the documented fail-loud
//! behaviour.
//!
//! ## How it differs from a production kernel
//!
//! It links only the `tairix-arch-riscv64` port (the SMP path needs no
//! `kernel/*` subsystem) and supplies its own `kernel_main`. The
//! QEMU-exit shortcut lives in this dedicated bin, never behind a Cargo
//! feature on the arch crate (fail closed).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, Ordering};

    use tairix_arch_api::{CpuId, SchedulerArch, SecondaryBringup};
    use tairix_arch_riscv64::fdt::Fdt;
    use tairix_arch_riscv64::{
        halt_current_hart, handle_panic_via_serial, preempt, qemu_exit, smp, trap, RiscvArch,
        RiscvArchStorage, SERIAL_SINK,
    };
    use tairix_log::{log, Event, EventId, Level};

    /// `u32` sentinel for "no IPI callback has fired yet".
    const NO_CPU: u32 = u32::MAX;

    /// The two-hart `virt` board uses hart ids `0` and `1`; OpenSBI may
    /// boot on *either* one (it picked hart 1 in testing), so the boot
    /// hart is read at runtime and the secondary is the other hart. The
    /// logical [`CpuId`] map is the identity over these hart ids, so a
    /// `CpuId` and its hart id coincide throughout this test.
    const HART_COUNT: u32 = 2;

    /// Stable audit-event ids for the QEMU transcript.
    const SMP_TEST_START: EventId = EventId(4210);
    const SMP_SECONDARY_UP: EventId = EventId(4211);
    const SMP_TEST_PASS: EventId = EventId(4212);
    const SMP_TEST_FAIL: EventId = EventId(4213);

    /// Failure finisher code: the secondary hart never came up.
    const FAIL_SECONDARY_START: u16 = 1;
    /// Failure finisher code: the IPI fired on the wrong hart.
    const FAIL_WRONG_HART: u16 = 2;
    /// Failure finisher code: the boot hart id was outside `0..HART_COUNT`.
    const FAIL_UNEXPECTED_HART: u16 = 3;

    /// Set to `1` by the secondary hart once its trap vector and
    /// supervisor-software-interrupt enable are in place.
    static SECONDARY_READY: AtomicU32 = AtomicU32::new(0);

    /// Count of IPI callbacks serviced (incremented on the hart that
    /// takes the software-interrupt trap).
    static IPI_COUNT: AtomicU32 = AtomicU32::new(0);

    /// The hart id the most recent IPI callback fired on; `NO_CPU` until
    /// one fires.
    static IPI_CPU: AtomicU32 = AtomicU32::new(NO_CPU);

    /// The IPI callback the software-interrupt trap path invokes. A real
    /// scheduler would request a reschedule on `cpu`; the test only
    /// needs to prove the IPI was delivered and dispatched on the right
    /// hart.
    extern "C" fn on_ipi(cpu: CpuId) {
        IPI_CPU.store(cpu, Ordering::SeqCst);
        IPI_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    /// Entry the secondary hart runs (via the `smp.s` trampoline) once
    /// the boot hart starts it. Enables interrupts, signals ready, and
    /// idles waiting for the IPI.
    extern "C" fn secondary_entry(_hartid: CpuId) -> ! {
        // Install the trap vector and enable supervisor software
        // interrupts on this hart so the delivered IPI traps to the
        // shared handler.
        // SAFETY: this is the secondary hart's first action; it has a
        // private stack (smp.s) and no source is armed yet. The shared
        // IPI callback was installed by the boot hart before it started
        // this hart.
        unsafe {
            trap::init_traps();
            preempt::enable_ipi();
        }
        // Publish readiness only after interrupts are enabled, so the
        // boot hart cannot send the IPI before this hart can take it.
        SECONDARY_READY.store(1, Ordering::SeqCst);

        loop {
            // SAFETY: `wfi` is a wait-for-interrupt hint with no
            // architectural side effects; the delivered IPI wakes it.
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
            }
        }
    }

    /// Forward to the shared riscv64 panic bridge (parks the hart; the
    /// run then times out and the harness reports the failure).
    #[panic_handler]
    fn tairix_ipi_smp_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s`
    /// trampoline calls (via `tairix_arch_riscv64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: SMP_TEST_START,
                message: "riscv64 IPI/SMP test: starting secondary hart",
                fields: &[],
            },
        );

        // Read the timer frequency for the arch handle. Fail closed
        // (park → timeout) if the device tree omits it.
        // SAFETY: `dtb` is the verbatim `a1` pointer OpenSBI handed the
        // boot hart; `boot.s` forwards it unchanged.
        let Some(timebase) = (unsafe { Fdt::from_ptr(dtb as *const u8) })
            .ok()
            .and_then(|f| f.timebase_frequency())
        else {
            halt_current_hart()
        };
        // OpenSBI may boot on either hart of the two-hart board; derive
        // the boot and secondary hart ids rather than assuming hart 0.
        #[allow(clippy::cast_possible_truncation)]
        let boot_hartid = hartid as CpuId;
        if boot_hartid >= HART_COUNT {
            qemu_exit::exit_failure(FAIL_UNEXPECTED_HART);
        }
        let secondary_hartid: CpuId = boot_hartid ^ 1;

        // Build the arch handle with the two-hart map up front so the
        // `SecondaryBringup` Arch HAL trait can map the dense `CpuId` to
        // the target hart id, `current_cpu` reverse-maps the running
        // hart, and `send_ipi` targets the right hart. The logical CPU
        // map is the identity over hart ids `0` and `1`.
        // Two-hart vertical: two per-CPU slots, owned by an allocator-free
        // `static` backing.
        static STORAGE: RiscvArchStorage<2> = RiscvArchStorage::new();
        let arch = RiscvArch::with_harts(&STORAGE, boot_hartid, timebase, &[0, 1]);

        // Install the shared callbacks before starting the secondary
        // hart, so it observes them already in place.
        preempt::set_ipi_callback(on_ipi);
        if smp::set_secondary_entry(secondary_entry).is_err() {
            qemu_exit::exit_failure(FAIL_SECONDARY_START);
        }

        // Register the secondary-hart stack pool sized to this two-hart
        // vertical before any `hart_start`; the `smp.s` trampoline reads
        // its published base/shift to seed the started hart's stack
        // (the pool scales with the hart count, not a
        // fixed `const`). The pool covers slots 0 and 1 so either hart can
        // be the started secondary.
        static SECONDARY_STACKS: smp::SecondaryStackPool<2> = smp::SecondaryStackPool::new();
        if SECONDARY_STACKS.register().is_err() {
            qemu_exit::exit_failure(FAIL_SECONDARY_START);
        }

        // Start the other hart through the `SecondaryBringup` Arch HAL
        // trait (`plans/WIRING.md` Stage W14/W15) rather than the
        // port-private `smp::start_secondary`, so this vertical exercises
        // the same neutral bring-up surface the x86_64 SMP verticals use.
        // SAFETY: called on the boot hart after the secondary stack pool
        // was registered (above) and after the secondary entry was
        // installed; `secondary_hartid` is a real, parked, distinct hart
        // the handle's topology map covers.
        if unsafe { arch.start_secondary(secondary_hartid) }.is_err() {
            qemu_exit::exit_failure(FAIL_SECONDARY_START);
        }

        // Wait until the secondary hart has enabled interrupts.
        while SECONDARY_READY.load(Ordering::SeqCst) == 0 {
            core::hint::spin_loop();
        }
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: SMP_SECONDARY_UP,
                message: "riscv64 IPI/SMP test: secondary hart up, sending IPI",
                fields: &[],
            },
        );

        // Send a directed IPI to the secondary hart through the arch
        // handle's SBI IPI path (the deliverable that replaces the former
        // `send_ipi` no-op). The logical CPU map is the identity over
        // hart ids `0` and `1`, so the target `CpuId` is the hart id.
        arch.send_ipi(secondary_hartid);

        // Wait for the secondary hart to take the software-interrupt
        // trap and run the callback.
        while IPI_COUNT.load(Ordering::SeqCst) == 0 {
            core::hint::spin_loop();
        }

        // The callback must have fired on the secondary hart, not the
        // boot hart.
        if IPI_CPU.load(Ordering::SeqCst) != secondary_hartid {
            log(
                &SERIAL_SINK,
                &Event {
                    level: Level::Error,
                    id: SMP_TEST_FAIL,
                    message: "riscv64 IPI/SMP test: IPI fired on the wrong hart",
                    fields: &[],
                },
            );
            qemu_exit::exit_failure(FAIL_WRONG_HART);
        }

        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: SMP_TEST_PASS,
                message: "riscv64 IPI/SMP test: IPI delivered to secondary hart",
                fields: &[],
            },
        );
        qemu_exit::exit_success();
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}

#[cfg(not(itest_riscv64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
