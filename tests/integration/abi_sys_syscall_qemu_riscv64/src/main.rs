//! CCOMPAT stage CC2 QEMU integration test: a full `lib/abi-sys` syscall
//! round-trip on the freestanding `riscv64gc-unknown-none-elf` target.
//!
//! ## What this test asserts
//!
//! The x86_64 sibling (`tairix-test-abi-sys-syscall-qemu`) can drive the
//! `abi-sys` stub straight from ring 0 because the `syscall` instruction
//! traps into the kernel entry path identically from any privilege level.
//! riscv64 has no such shortcut: the kernel routes only an `ecall` *from
//! U-mode* (`scause` `SCAUSE_ECALL_FROM_U`) to the syscall dispatch
//! callback (`kernel/arch/riscv64/src/syscall_entry.rs`). So this test
//! stands up a minimal U-mode context and issues the stub from there,
//! exercising the real `ecall` (`lib/abi-sys/src/trap.rs`) end-to-end.
//!
//! ## How it asserts it
//!
//! Using the Stage-3 Sv39 primitives (`tairix_arch_riscv64::paging`) it:
//!
//! 1. Builds one `AddressSpace` that identity-maps the low 4 GiB with
//!    1 GiB leaves (R|W|X, no U bit) so the kernel's own code/stack/data,
//!    the trap vector, and the `virt` board's MMIO stay reachable in
//!    S-mode.
//! 2. Aliases the page(s) holding the `tairix_sys_cap_query` stub at a high
//!    user virtual address (`USER_CODE_VA`) with the **U** bit set
//!    (U|R|X), and maps a small user stack at `USER_STACK_VA` (U|R|W).
//!    The stub is a self-contained leaf (it marshals registers and
//!    executes `ecall`, calling nothing), so a single-page code alias is
//!    sufficient. The identity pages carry no U bit, so U-mode can reach
//!    *only* the aliased stub and its stack — the isolation contract.
//! 3. Installs the syscall dispatch callback via
//!    `set_dispatch_callback`, points `stvec` at the trap vector
//!    (`trap::init_traps`), sets `sstatus.SUM` (so the S-mode trap
//!    handler may touch the U-bit user stack), and `sret`s to U-mode at
//!    the aliased stub entry with the capability id in `a0`.
//!
//! The stub's `ecall` raises an environment-call-from-U exception into
//! the kernel S-mode trap vector, which marshals `a7`/`a0`–`a5` into the
//! canonical `[u64; SYSCALL_MAX_ARGS]` layout and calls the installed
//! callback. `record_and_exit` therefore observes the register
//! marshalling end-to-end: it asserts the dispatched number is
//! `SyscallNumber::CAP_QUERY` and that argument 0 is the capability id the
//! stub was handed (the rest zero), then writes the `SiFive` Test PASS
//! finisher. A wrong number, a wrong argument, or the `ecall` resuming in
//! U-mode at all is a distinct closed failure.
//!
//! ## How it differs from `tairix-test-syscall-dispatch-qemu`
//!
//! That test drives `Dispatcher::dispatch` directly and never executes a
//! trap instruction. This test issues the `abi-sys` stub from U-mode, so
//! the `ecall` instruction and the kernel's trap vector are exercised
//! together — the riscv64 half of the CC2 deliverable in
//! `plans/CCOMPAT.md`.
//!
//! ## `test-hooks` Cargo feature
//!
//! The test body only compiles under `#[cfg(feature = "test-hooks")]`.
//! The feature is on by default for this crate; release builds that
//! enable it are rejected by the `compile_error!` guard below
//! (no hacks; — fail closed), mirroring
//! `tairix-test-abi-sys-syscall-qemu`.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// — test affordances must never reach a release binary.
// `test-hooks` is on by default for this crate; a release build that
// re-enables it is a configuration error, so we fail the build outright,
// exactly as `tairix-test-abi-sys-syscall-qemu` does.
#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-abi-sys-syscall-qemu-riscv64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

#[cfg(all(itest_riscv64, feature = "test-hooks"))]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, Ordering};

    use tairix_abi::{CapabilityId, SyscallNumber, SYSCALL_MAX_ARGS};
    use tairix_arch_api::{EnterUser, UserEntry};
    use tairix_arch_riscv64::{
        handle_panic_via_serial, paging, qemu_exit, syscall_entry, trap, userentry::UserMode,
        SERIAL_SINK,
    };
    use tairix_log::{log, Event, EventId, Level};

    /// Capability id `kernel_main` passes to `tairix_sys_cap_query` and
    /// [`record_and_exit`] expects to see marshalled into argument 0. Any
    /// well-known [`CapabilityId`] works — the test asserts the stub's
    /// *marshalling*, not the kernel's grant decision (the dispatch
    /// callback is intercepted before any grant evaluation).
    const EXPECTED_CAP: CapabilityId = CapabilityId::TIME_SET;

    /// Gigapages of identity map the S-mode space holds: `[0, 4 GiB)`
    /// covers the `virt` board's low MMIO (`SiFive` Test, PLIC, …) and the
    /// 2 GiB RAM base at `0x8000_0000` where this kernel runs.
    const IDENTITY_GIGABYTES: usize = 4;

    /// User virtual address the stub code is aliased at. Chosen at 64 GiB
    /// — far above the 4 GiB identity window — so the alias lands on
    /// freshly-walked 4 KiB tables rather than colliding with an identity
    /// gigapage leaf. Page aligned and canonical for Sv39 (bit 38 clear).
    const USER_CODE_VA: u64 = 0x10_0000_0000;

    /// User virtual address the stack is mapped at (4 MiB above the code
    /// alias so the two never share a table leaf). Page aligned.
    const USER_STACK_VA: u64 = 0x10_0040_0000;

    /// User stack size in 4 KiB pages (16 KiB — ample for the stub
    /// prologue and the S-mode trap handler that runs on it after the
    /// `ecall`).
    const USER_STACK_PAGES: usize = 4;

    /// Stable audit-event ids for the QEMU transcript.
    const TEST_START: EventId = EventId(4220);
    const TEST_FAIL: EventId = EventId(4222);

    /// `SiFive` Test failure codes, distinct per failure site so a
    /// failing run's exit status pinpoints the broken invariant.
    const FAIL_POOL: u16 = 1;
    const FAIL_WRONG_NUMBER: u16 = 2;
    const FAIL_WRONG_ARGS: u16 = 3;
    const FAIL_ECALL_RETURNED: u16 = 4;

    /// Page-table pool backing the address space (lives in `.bss`).
    static PAGE_TABLE_POOL: paging::PageTablePool = paging::PageTablePool::new();

    /// Backing storage for the U-mode stack. `align(4096)` so each page
    /// maps to a valid frame; `USER_STACK_PAGES` pages.
    #[repr(C, align(4096))]
    struct UserStack([u8; paging::PAGE_SIZE * USER_STACK_PAGES]);

    static mut USER_STACK: UserStack = UserStack([0; paging::PAGE_SIZE * USER_STACK_PAGES]);

    /// Set once the round-trip has been driven so a re-entry can never
    /// re-run the test logic (fail closed).
    static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

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
    fn tairix_abi_sys_syscall_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// The syscall dispatch callback installed for the round-trip.
    ///
    /// Reached from the kernel's S-mode trap vector after the U-mode stub
    /// executed `ecall`. It asserts the marshalled `(number, args)` match
    /// what `tairix_sys_cap_query(EXPECTED_CAP)` should have placed in the
    /// registers, then writes the PASS finisher. It never returns to the
    /// caller (it diverges through `qemu_exit`): returning would advance
    /// `sepc` and `sret` back into U-mode.
    ///
    /// The signature matches `syscall_entry::SyscallDispatchFn`.
    extern "C" fn record_and_exit(number: u64, args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64 {
        // SAFETY: the trap vector built the `[u64; SYSCALL_MAX_ARGS]`
        // array and passes a pointer valid for the duration of this call
        // (`syscall_entry` contract).
        let args = unsafe { *args_ptr };

        let expected_number = u64::from(SyscallNumber::CAP_QUERY.as_u16());
        if number != expected_number {
            note(TEST_FAIL, "dispatched the wrong syscall number");
            qemu_exit::exit_failure(FAIL_WRONG_NUMBER);
        }

        let expected_arg0 = u64::from(EXPECTED_CAP.as_u16());
        if args[0] != expected_arg0 || args[1..] != [0, 0, 0, 0, 0] {
            note(TEST_FAIL, "dispatched the wrong argument vector");
            qemu_exit::exit_failure(FAIL_WRONG_ARGS);
        }

        qemu_exit::exit_success();
    }

    /// Boot entry point — the symbol the arch crate's boot trampoline
    /// calls (via `tairix_arch_riscv64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_hartid: u64, _dtb: u64) -> ! {
        if TEST_DRIVEN
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // A second entry can only mean a kernel bug; fail closed.
            qemu_exit::exit_failure(FAIL_POOL);
        }

        note(
            TEST_START,
            "riscv64 abi-sys round-trip: building the U-mode context",
        );

        let page = paging::PAGE_SIZE as u64;

        let Some(mut space) =
            paging::AddressSpace::new_identity_gigapages(&PAGE_TABLE_POOL, IDENTITY_GIGABYTES)
        else {
            note(TEST_FAIL, "page-table pool exhausted (identity map)");
            qemu_exit::exit_failure(FAIL_POOL);
        };

        // ---- Alias the stub's page(s) into U-mode (U|R|X). ----
        let func = tairix_abi_sys::sys_cap_query as *const () as u64;
        let func_page = func & !(page - 1);
        let code_flags = paging::flags::USER | paging::flags::READ | paging::flags::EXEC;
        // Map the stub's page plus the following one: the stub is far
        // smaller than a page, but mapping two pages is cheap insurance
        // against it straddling a page boundary.
        for i in 0..2u64 {
            if space
                .map_4k(
                    &PAGE_TABLE_POOL,
                    USER_CODE_VA + i * page,
                    func_page + i * page,
                    code_flags,
                )
                .is_none()
            {
                note(TEST_FAIL, "page-table pool exhausted (code alias)");
                qemu_exit::exit_failure(FAIL_POOL);
            }
        }
        let user_entry = USER_CODE_VA | (func & (page - 1));

        // ---- Map the U-mode stack (U|R|W). ----
        let stack_phys = core::ptr::addr_of!(USER_STACK) as u64;
        let stack_flags = paging::flags::USER | paging::flags::READ | paging::flags::WRITE;
        for i in 0..USER_STACK_PAGES as u64 {
            if space
                .map_4k(
                    &PAGE_TABLE_POOL,
                    USER_STACK_VA + i * page,
                    stack_phys + i * page,
                    stack_flags,
                )
                .is_none()
            {
                note(TEST_FAIL, "page-table pool exhausted (stack)");
                qemu_exit::exit_failure(FAIL_POOL);
            }
        }
        let user_sp = USER_STACK_VA + (USER_STACK_PAGES as u64) * page;

        // ---- Install the dispatch callback and the trap vector. ----
        syscall_entry::set_dispatch_callback(record_and_exit);

        // SAFETY: the identity space maps the current `pc` and `sp`, so
        // the `satp` switch is sound.
        unsafe { space.switch() };
        // SAFETY: called once on the boot hart with a stack established
        // and the dispatch callback installed; no interrupt source is
        // armed, so only the deliberate `ecall` below reaches the vector.
        unsafe { trap::init_traps() };

        note(
            TEST_START,
            "dropping to U-mode to issue tairix_sys_cap_query",
        );

        // ---- Drop to U-mode and issue the stub via the Arch HAL. ----
        // SAFETY: `user_entry` aliases the executable U|R|X stub page and
        // `user_sp` tops the U|R|W stack, both mapped above; the dispatch
        // callback and trap vector are installed. The `sret` sequence is
        // the one HAL definition (`tairix_arch_riscv64::userentry`).
        unsafe {
            UserMode::new().enter_user(UserEntry::new(
                user_entry,
                user_sp,
                u64::from(EXPECTED_CAP.as_u16()),
            ))
        }

        // `enter_user_mode` diverges via `sret`; this point is reached
        // only if the `ecall` resumed in U-mode and the stub returned to a
        // caller that fell through — neither can happen here, so it is a
        // closed failure.
        #[allow(unreachable_code)]
        {
            note(TEST_FAIL, "ecall resumed in U-mode unexpectedly");
            qemu_exit::exit_failure(FAIL_ECALL_RETURNED);
        }
    }
}

// --- Stub when the test-hooks feature is off ----------------------
//
// The test body only compiles when `feature = "test-hooks"` is on.
// Disabling it leaves the bin as a no-op so a layout sanity check
// (`cargo build --no-default-features`) still builds
// (a disabled test must compile cleanly).
#[cfg(all(itest_riscv64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_hartid: u64, _dtb: u64) -> ! {
    loop {
        // SAFETY: `wfi` is a well-defined parked-hart hint on riscv64. Looping defends against spurious wake-ups.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(all(itest_riscv64, not(feature = "test-hooks")))]
#[panic_handler]
fn tairix_abi_sys_syscall_qemu_riscv64_panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        // SAFETY: same as above.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}

#[cfg(not(itest_riscv64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
