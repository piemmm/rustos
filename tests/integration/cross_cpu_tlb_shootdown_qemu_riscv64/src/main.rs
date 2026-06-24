//! WIRING Stage W6 QEMU integration test: cross-CPU TLB shootdown on the
//! riscv64 `virt` board.
//!
//! ## What this test asserts
//!
//! The `rustos_arch_api::CrossCpuTlbShootdown` HAL slice requires that a
//! page-table edit on one CPU can be made visible on the others. riscv64
//! has no broadcast `sfence.vma`, so `RiscvArch`'s implementation issues
//! a local `sfence.vma` plus the SBI **RFENCE** `remote_sfence_vma`
//! firmware call to every other online hart. This binary proves that path
//! end to end on a two-hart `virt` board:
//!
//! 1. The boot hart starts the other hart via `smp::start_secondary` (the
//!    SBI HSM `hart_start` call); the secondary signals `READY` and idles.
//! 2. The boot hart drives `RiscvArch::shootdown_page`, which runs the
//!    local `sfence.vma` and the SBI `remote_sfence_vma` to the live
//!    secondary hart — proving the new cross-CPU code path executes on a
//!    real multi-hart machine without trapping.
//! 3. To confirm the firmware actually *honours* the remote fence (rather
//!    than silently no-op'ing an unimplemented extension), the boot hart
//!    issues the SBI `remote_sfence_vma` directly and checks the returned
//!    `sbi::SbiRet` reports success. OpenSBI services the fence by
//!    waking the target hart with an M-mode IPI and executing `sfence.vma`
//!    there, so a success return means the remote hart was reached.
//!
//! A regression that fails to start the hart, traps in `shootdown_page`,
//! or whose firmware rejects the remote fence never reaches the PASS
//! write, so the run times out or trips a failure finisher — the
//! documented fail-loud behaviour.
//!
//! ## How it differs from a production kernel
//!
//! It links only the `rustos-arch-riscv64` port and supplies its own
//! `kernel_main`. The QEMU-exit shortcut lives in this dedicated bin,
//! never behind a Cargo feature on the arch crate (fail closed).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, Ordering};

    use rustos_arch_api::{CpuId, CrossCpuTlbShootdown};
    use rustos_arch_riscv64::fdt::Fdt;
    use rustos_arch_riscv64::{
        halt_current_hart, handle_panic_via_serial, qemu_exit, sbi, smp, RiscvArch,
        RiscvArchStorage, SERIAL_SINK,
    };
    use rustos_log::{log, Event, EventId, Level};

    /// The two-hart `virt` board uses hart ids `0` and `1`; OpenSBI may
    /// boot on *either* one, so the boot hart is read at runtime and the
    /// secondary is the other hart. The logical `CpuId` map is the
    /// identity over these hart ids.
    const HART_COUNT: u32 = 2;

    /// A representative page to invalidate. The exact address is
    /// immaterial — a TLB shootdown can only ever *over*-invalidate — so
    /// any 4 KiB-aligned value in the kernel window will do.
    const SHOOTDOWN_VADDR: u64 = 0x8020_0000;

    /// 4 KiB page size (matches the Sv39 leaf size).
    const PAGE_SIZE: usize = 4096;

    /// Stable audit-event ids for the QEMU transcript.
    const TEST_START: EventId = EventId(4260);
    const SECONDARY_UP: EventId = EventId(4261);
    const TEST_PASS: EventId = EventId(4262);
    const TEST_FAIL: EventId = EventId(4263);

    /// Failure finisher code: the secondary hart never came up.
    const FAIL_SECONDARY_START: u16 = 1;
    /// Failure finisher code: the SBI remote fence reported an error.
    const FAIL_RFENCE_ERROR: u16 = 2;
    /// Failure finisher code: the boot hart id was outside `0..HART_COUNT`.
    const FAIL_UNEXPECTED_HART: u16 = 3;

    /// Set to `1` by the secondary hart once it is up and idling, so the
    /// boot hart only shoots down against a live remote hart.
    static SECONDARY_READY: AtomicU32 = AtomicU32::new(0);

    /// Entry the secondary hart runs (via the `smp.s` trampoline). It only
    /// needs to be *running* for the firmware remote fence to reach it
    /// (OpenSBI wakes it with an M-mode IPI), so it signals ready and
    /// idles — no S-mode trap setup is required.
    extern "C" fn secondary_entry(_hartid: CpuId) -> ! {
        SECONDARY_READY.store(1, Ordering::SeqCst);
        loop {
            // SAFETY: `wfi` is a wait-for-interrupt hint with no
            // architectural side effects; the firmware's M-mode IPI for
            // the remote fence wakes it.
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
            }
        }
    }

    /// Forward to the shared riscv64 panic bridge (parks the hart; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn rustos_xtlb_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_riscv64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: TEST_START,
                message: "riscv64 cross-CPU TLB shootdown test: starting secondary hart",
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

        // Install the secondary entry, register the stack pool, then start
        // the other hart. The pool is sized to this two-hart vertical
        // (slots 0 and 1, so either hart can be the started secondary) and
        // scales with the hart count, not a fixed `const`; the `smp.s` trampoline reads its published base/shift.
        if smp::set_secondary_entry(secondary_entry).is_err() {
            qemu_exit::exit_failure(FAIL_SECONDARY_START);
        }
        static SECONDARY_STACKS: smp::SecondaryStackPool<2> = smp::SecondaryStackPool::new();
        if SECONDARY_STACKS.register().is_err() {
            qemu_exit::exit_failure(FAIL_SECONDARY_START);
        }
        // SAFETY: called on the boot hart after the secondary stack pool
        // was registered (above) and after the secondary entry was
        // installed; `secondary_hartid` is a real, parked, distinct hart.
        if unsafe { smp::start_secondary(secondary_hartid) }.is_err() {
            qemu_exit::exit_failure(FAIL_SECONDARY_START);
        }

        // Wait until the secondary hart is up, so the remote fence has a
        // live target.
        while SECONDARY_READY.load(Ordering::SeqCst) == 0 {
            core::hint::spin_loop();
        }
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: SECONDARY_UP,
                message: "riscv64 cross-CPU TLB shootdown test: secondary up, shooting down",
                fields: &[],
            },
        );

        // Drive the real HAL entry point: a local `sfence.vma` plus the
        // SBI `remote_sfence_vma` to the secondary hart. Reaching the next
        // line proves the new cross-CPU code path ran on a real two-hart
        // machine without trapping.
        // Two-hart vertical: two per-CPU slots, owned by an allocator-free
        // `static` backing.
        static STORAGE: RiscvArchStorage<2> = RiscvArchStorage::new();
        let arch = RiscvArch::with_harts(&STORAGE, boot_hartid, timebase, &[0, 1]);
        arch.shootdown_page(SHOOTDOWN_VADDR);

        // Confirm the firmware *honours* the remote fence (not a silent
        // no-op for an unimplemented extension): issue the SBI call
        // directly for the secondary hart and require success.
        let (mask, base) = sbi::hart_mask_for(secondary_hartid);
        let page = SHOOTDOWN_VADDR & !(PAGE_SIZE as u64 - 1);
        let ret = sbi::remote_sfence_vma(mask, base, page as usize, PAGE_SIZE);
        if !ret.is_success() {
            log(
                &SERIAL_SINK,
                &Event {
                    level: Level::Error,
                    id: TEST_FAIL,
                    message: "riscv64 cross-CPU TLB shootdown test: SBI remote fence failed",
                    fields: &[],
                },
            );
            qemu_exit::exit_failure(FAIL_RFENCE_ERROR);
        }

        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: TEST_PASS,
                message:
                    "riscv64 cross-CPU TLB shootdown test: remote fence reached secondary hart",
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
