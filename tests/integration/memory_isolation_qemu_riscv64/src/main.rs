//! Stage 3c QEMU integration test: hardware-enforced memory isolation on
//! riscv64.
//!
//! ## What this test asserts
//!
//! states that "memory isolation is enforced by hardware
//! (page tables / MMU / WASM sandboxing). A process can only reach
//! another process's memory through an explicit, capability-checked
//! shared-memory IPC object." This binary is the riscv64 analogue of
//! `tests/integration/memory_isolation` (`x86_64`): it makes that promise
//! concrete at the Sv39 page-table layer on the QEMU `virt` board, and
//! is the Stage-3 "memory-isolation test passes" per-sub-stage
//! deliverable for riscv64.
//!
//! ## How it asserts it
//!
//! Two distinct `AddressSpace`s are constructed from
//! `rustos_arch_riscv64::paging` (the Stage-3 Sv39 primitives). Each
//! identity-maps the low 4 GiB with 1 GiB leaves so the kernel's own
//! code/stack/data and the board's MMIO stay reachable whichever space
//! is active:
//!
//! * **Victim** additionally maps a single 4 KiB frame at the secret
//!   virtual address `SECRET_VADDR` (well above the 4 GiB identity
//!   window) to a physical frame initialised with `SECRET_BYTE`.
//! * **Attacker** maps the identity window only; `SECRET_VADDR` is
//!   unmapped at every level of its Sv39 hierarchy.
//!
//! The boot hart then:
//!
//! 1. Switches `satp` to the *victim* space, reads `SECRET_VADDR`, and
//!    asserts the byte is intact (proving the mapping was real).
//! 2. Installs a `fault` handler and the S-mode trap vector, switches to
//!    the *attacker* space, and reads `SECRET_VADDR`.
//! 3. The hart raises a **load page fault** (`scause` 13); the trap
//!    vector routes it to `on_fault`, which validates that (a) the
//!    cause is exactly a load page fault, (b) `stval` (the faulting
//!    address) equals `SECRET_VADDR`, and (c) the victim's frame is
//!    still readable through its identity-mapped physical address — the
//!    attack neither succeeded nor corrupted the victim — then writes
//!    the `SiFive` Test PASS finisher.
//!
//! Any other outcome (no fault, wrong cause, wrong `stval`, corrupted
//! victim) is a closed failure (fail closed). A
//! regression that maps the attacker's `SECRET_VADDR` never faults and
//! falls through to the failure finisher.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_riscv64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use rustos_arch_api::mmu::{AddressSpace as _, PageFlags};
    use rustos_arch_riscv64::{
        fault, handle_panic_via_serial, paging, qemu_exit, trap, SERIAL_SINK,
    };
    use rustos_log::{log, Event, EventId, Level};

    /// Virtual address only the *victim* space maps. Chosen at 64 GiB —
    /// far above the 4 GiB identity window both spaces share — so the
    /// attacker space leaves it unmapped at every Sv39 level. Page
    /// aligned and canonical for Sv39 (bit 38 clear).
    const SECRET_VADDR: u64 = 0x10_0000_0000;

    /// Magic byte written into the secret frame.
    const SECRET_BYTE: u8 = 0xC0;

    /// Gigapages of identity map both spaces share: `[0, 4 GiB)` covers
    /// the `virt` board's low MMIO (the `SiFive` Test device, PLIC, …)
    /// and the 2 GiB RAM base at `0x8000_0000` where this kernel runs.
    const IDENTITY_GIGABYTES: usize = 4;

    /// Stable audit-event ids for the QEMU transcript.
    const TEST_START: EventId = EventId(4210);
    const TEST_PASS: EventId = EventId(4211);
    const TEST_FAIL: EventId = EventId(4212);

    /// `SiFive` Test failure codes, distinct per failure site so a
    /// failing run's exit status pinpoints the broken invariant.
    const FAIL_POOL: u16 = 1;
    const FAIL_VICTIM_WRONG_BYTE: u16 = 2;
    const FAIL_ATTACKER_NO_FAULT: u16 = 3;
    const FAIL_FAULT_BEFORE_ATTACK: u16 = 4;
    const FAIL_WRONG_CAUSE: u16 = 5;
    const FAIL_WRONG_STVAL: u16 = 6;
    const FAIL_VICTIM_CORRUPTED: u16 = 7;

    /// Page-table pool backing both address spaces (lives in `.bss`).
    static PAGE_TABLE_POOL: paging::PageTablePool = paging::PageTablePool::new();

    /// 4 KiB frame the victim maps at [`SECRET_VADDR`]. `align(4096)` so
    /// its physical address is a valid page frame.
    #[repr(C, align(4096))]
    struct SecretFrame([u8; paging::PAGE_SIZE]);

    static mut SECRET_FRAME: SecretFrame = SecretFrame([0; paging::PAGE_SIZE]);

    /// `true` once the attacker space is active — lets [`on_fault`] tell
    /// an *expected* fault from a kernel bug that faults earlier.
    static ATTACKER_ACTIVE: AtomicBool = AtomicBool::new(false);

    /// Physical address of the secret frame, recorded once after set-up
    /// so [`on_fault`] can confirm the victim's data survived.
    static SECRET_PHYS: AtomicU64 = AtomicU64::new(0);

    fn note(id: EventId, message: &'static str) {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id,
                message,
                fields: &[],
            },
        );
    }

    /// Forward to the shared riscv64 panic bridge (parks the hart; the
    /// run then times out and the harness reports the failure).
    #[panic_handler]
    fn rustos_memory_isolation_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// The synchronous-exception handler the trap vector invokes. The
    /// attacker's read of the unmapped [`SECRET_VADDR`] must land here as
    /// a load page fault; anything else is a closed failure.
    extern "C" fn on_fault(scause: u64, stval: u64, _sepc: u64) -> ! {
        if !ATTACKER_ACTIVE.load(Ordering::SeqCst) {
            note(TEST_FAIL, "fault before attacker switch — kernel bug");
            qemu_exit::exit_failure(FAIL_FAULT_BEFORE_ATTACK);
        }
        if scause != fault::SCAUSE_LOAD_PAGE_FAULT {
            note(TEST_FAIL, "unexpected trap cause, not a load page fault");
            qemu_exit::exit_failure(FAIL_WRONG_CAUSE);
        }
        if stval != SECRET_VADDR {
            note(TEST_FAIL, "load page fault at the wrong address");
            qemu_exit::exit_failure(FAIL_WRONG_STVAL);
        }

        // The victim's data must still be intact at its physical address
        // (identity-mapped under the attacker space too), proving the
        // attack neither read nor corrupted it.
        let secret_phys = SECRET_PHYS.load(Ordering::SeqCst);
        // SAFETY: `secret_phys` is `SECRET_FRAME`'s address, inside the
        // identity-mapped low 4 GiB, so it dereferences directly under
        // the active attacker `satp`.
        let victim_byte = unsafe { core::ptr::read_volatile(secret_phys as *const u8) };
        if victim_byte != SECRET_BYTE {
            note(TEST_FAIL, "victim frame corrupted by the attack");
            qemu_exit::exit_failure(FAIL_VICTIM_CORRUPTED);
        }

        note(TEST_PASS, "attacker faulted, victim intact");
        qemu_exit::exit_success();
    }

    /// Boot entry point — the symbol the arch crate's `boot.s`
    /// trampoline calls (via `rustos_arch_riscv64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_hartid: u64, _dtb: u64) -> ! {
        note(
            TEST_START,
            "riscv64 memory-isolation test: building Sv39 spaces",
        );

        // Stash the secret byte into its frame while paging is still off
        // (the boot slice runs with `satp` unset), recording its physical
        // address for the fault handler's victim-intact check.
        let secret_phys = core::ptr::addr_of!(SECRET_FRAME) as u64;
        SECRET_PHYS.store(secret_phys, Ordering::SeqCst);
        // SAFETY: `SECRET_FRAME` is owned exclusively by this single-hart
        // boot-time set-up; the matching read in `on_fault` happens after
        // this store. The raw pointer avoids a `&mut` to the static.
        unsafe {
            core::ptr::addr_of_mut!(SECRET_FRAME)
                .cast::<u8>()
                .write_volatile(SECRET_BYTE);
        }

        // ---- Build the victim space and add the secret mapping. ----
        let Some(mut victim) =
            paging::AddressSpace::new_identity_gigapages(&PAGE_TABLE_POOL, IDENTITY_GIGABYTES)
        else {
            note(TEST_FAIL, "page-table pool exhausted (victim)");
            qemu_exit::exit_failure(FAIL_POOL);
        };
        // Install the secret mapping through the Arch HAL MMU surface
        // (`rustos_arch_api::mmu::AddressSpace::map_page`), the path
        // the architecture-neutral kernel uses, rather than the port's
        // inherent `map_4k` (`plans/WIRING.md` W5b).
        if victim
            .map_page(
                SECRET_VADDR,
                secret_phys,
                PageFlags::READ | PageFlags::WRITE,
            )
            .is_err()
        {
            note(TEST_FAIL, "secret mapping refused");
            qemu_exit::exit_failure(FAIL_POOL);
        }

        // ---- Build the attacker space: identity window only. ----
        let Some(attacker) =
            paging::AddressSpace::new_identity_gigapages(&PAGE_TABLE_POOL, IDENTITY_GIGABYTES)
        else {
            note(TEST_FAIL, "page-table pool exhausted (attacker)");
            qemu_exit::exit_failure(FAIL_POOL);
        };

        // ---- Phase 1: confirm the victim mapping is genuine. ----
        // SAFETY: the victim identity-maps the low 4 GiB, covering the
        // current `pc` and stack, so the switch is sound.
        unsafe { victim.activate() };
        // SAFETY: `SECRET_VADDR` is mapped read/write in the victim space.
        let seen = unsafe { core::ptr::read_volatile(SECRET_VADDR as *const u8) };
        if seen != SECRET_BYTE {
            note(TEST_FAIL, "victim observed the wrong secret byte");
            qemu_exit::exit_failure(FAIL_VICTIM_WRONG_BYTE);
        }
        note(TEST_START, "victim sees the secret; switching to attacker");

        // ---- Phase 2: arm the fault path and switch to the attacker. ----
        if fault::set_fault_handler(on_fault).is_err() {
            note(TEST_FAIL, "fault handler already installed");
            qemu_exit::exit_failure(FAIL_POOL);
        }
        // SAFETY: called once on the boot hart with a stack established
        // and the fault handler installed; no interrupt source is armed,
        // so only the synchronous page fault below reaches the vector.
        unsafe { trap::init_traps() };

        ATTACKER_ACTIVE.store(true, Ordering::SeqCst);
        // SAFETY: the attacker also identity-maps the low 4 GiB, covering
        // the current `pc` and stack.
        unsafe { attacker.activate() };

        // The next read MUST fault into `on_fault`. If it returns, the
        // attacker reached the victim-only frame — a broken kernel.
        // SAFETY: we *want* the fault; `on_fault` diverges to `qemu_exit`
        // so this volatile read never observably completes.
        let leaked = unsafe { core::ptr::read_volatile(SECRET_VADDR as *const u8) };
        let _ = leaked;

        note(
            TEST_FAIL,
            "attacker read the isolated address without faulting",
        );
        qemu_exit::exit_failure(FAIL_ATTACKER_NO_FAULT);
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}

#[cfg(not(itest_riscv64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
