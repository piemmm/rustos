//! CCOMPAT stage CC2 QEMU integration test: a full `lib/abi-sys` syscall
//! round-trip on the freestanding `aarch64-unknown-none` target.
//!
//! ## What this test asserts
//!
//! The x86_64 sibling (`tairix-test-abi-sys-syscall-qemu`) can drive the
//! `abi-sys` stub straight from ring 0 because the `syscall` instruction
//! traps into the kernel entry path identically from any privilege level.
//! aarch64 has no such shortcut: the kernel routes only an `svc` *from
//! EL0* (a lower-EL synchronous exception) to the syscall dispatch
//! callback (`kernel/arch/aarch64/src/exceptions.rs`). So this test
//! stands up a minimal EL0 context and issues the stub from there,
//! exercising the real `svc` (`lib/abi-sys/src/trap.rs`) end-to-end.
//!
//! ## How it asserts it
//!
//! Using the Stage-3 stage-1 paging primitives
//! (`tairix_arch_aarch64::paging`) it:
//!
//! 1. Builds one `AddressSpace` that identity-maps the low 2 GiB (device
//!    MMIO + RAM) so the kernel's own code/stack and the EL1 vector table
//!    stay reachable.
//! 2. Aliases the page(s) holding the `tairix_sys_cap_query` stub at a high
//!    user virtual address (`USER_CODE_VA`) with EL0-executable
//!    attributes (`el0_code_leaf_attrs`: EL0 read+execute, privileged
//!    execute-never), and maps a small EL0 stack at `USER_STACK_VA`
//!    (`el0_data_leaf_attrs`: EL0 read/write). The stub is a
//!    self-contained leaf (it marshals registers and executes `svc`,
//!    calling nothing), so a single-page code alias is sufficient. The
//!    identity pages are EL1-only, so EL0 can reach *only* the aliased
//!    stub and its stack — the isolation contract.
//! 3. Installs the syscall dispatch callback via `set_dispatch_callback`,
//!    points `VBAR_EL1` at the vector table (`init_vectors`), and `eret`s
//!    to EL0 with `SP_EL0` at the user stack, `ELR_EL1` at the stub, and
//!    the capability id in `x0`.
//!
//! The stub's `svc` raises a lower-EL synchronous exception into the EL1
//! vector, whose trampoline saves the GP register frame and the handler
//! marshals `x0`–`x5`/`x8` into the canonical `[u64; SYSCALL_MAX_ARGS]`
//! layout and calls the installed callback. `record_and_exit` therefore
//! observes the register marshalling end-to-end: it asserts the
//! dispatched number is `SyscallNumber::CAP_QUERY` and that argument 0 is
//! the capability id the stub was handed (the rest zero), then reports
//! PASS through the ARM semihosting finisher. A wrong number, a wrong
//! argument, or the `svc` resuming in EL0 at all is a distinct closed
//! failure.
//!
//! ## How it differs from `tairix-test-syscall-dispatch-qemu`
//!
//! That test drives `Dispatcher::dispatch` directly and never executes a
//! trap instruction. This test issues the `abi-sys` stub from EL0, so the
//! `svc` instruction and the kernel's EL1 vector are exercised together —
//! the aarch64 half of the CC2 deliverable in `plans/CCOMPAT.md`.
//!
//! ## `test-hooks` Cargo feature
//!
//! The test body only compiles under `#[cfg(feature = "test-hooks")]`.
//! The feature is on by default for this crate; release builds that
//! enable it are rejected by the `compile_error!` guard below
//! (no hacks; — fail closed), mirroring
//! `tairix-test-abi-sys-syscall-qemu`.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// Test affordances must never reach a release binary.
// `test-hooks` is on by default for this crate; a release build that
// re-enables it is a configuration error, so we fail the build outright,
// exactly as `tairix-test-abi-sys-syscall-qemu` does.
#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-abi-sys-syscall-qemu-aarch64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

#[cfg(all(itest_aarch64, feature = "test-hooks"))]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, Ordering};

    use tairix_abi::{CapabilityId, SyscallNumber, SYSCALL_MAX_ARGS};
    use tairix_arch_aarch64::paging::{
        el0_code_leaf_attrs, el0_data_leaf_attrs, AddressSpace, PageTablePool, PAGE_SIZE,
    };
    use tairix_arch_aarch64::{
        exceptions, handle_panic_via_serial, qemu_exit, syscall_entry, userentry::UserMode,
        SERIAL_SINK,
    };
    use tairix_arch_api::{EnterUser, UserEntry};
    use tairix_log::{log, Event, EventId, Level};

    /// Capability id `kernel_main` passes to `tairix_sys_cap_query` and
    /// [`record_and_exit`] expects to see marshalled into argument 0. Any
    /// well-known [`CapabilityId`] works — the test asserts the stub's
    /// *marshalling*, not the kernel's grant decision (the dispatch
    /// callback is intercepted before any grant evaluation).
    const EXPECTED_CAP: CapabilityId = CapabilityId::TIME_SET;

    /// GiB of identity map the EL1 space holds: `[0, 2 GiB)` covers the
    /// `virt` board's device MMIO (GiB 0) and the RAM base at GiB 1 where
    /// this kernel runs.
    const IDENTITY_GIB: usize = 2;

    /// User virtual address the stub code is aliased at. 64 GiB — well
    /// above the identity window — so the walk uses fresh L2/L3 tables
    /// rather than shattering an identity block. Within the 39-bit
    /// (512 GiB) TTBR0 region.
    const USER_CODE_VA: u64 = 64 << 30;

    /// User virtual address the EL0 stack is mapped at (4 MiB above the
    /// code alias). Page aligned.
    const USER_STACK_VA: u64 = (64 << 30) + (4 << 20);

    /// EL0 stack size in 4 KiB pages (16 KiB — ample for the stub
    /// prologue; the EL1 trap handler runs on `SP_EL1`, not this stack).
    const USER_STACK_PAGES: usize = 4;

    /// Stable audit-event ids for the QEMU transcript.
    const TEST_START: EventId = EventId(4240);
    const TEST_FAIL: EventId = EventId(4242);

    /// Semihosting failure codes, distinct per failure site so a failing
    /// run's exit status pinpoints the broken invariant.
    const FAIL_SETUP: u16 = 1;
    const FAIL_WRONG_NUMBER: u16 = 2;
    const FAIL_WRONG_ARGS: u16 = 3;
    const FAIL_SVC_RETURNED: u16 = 4;

    /// Page-table pool backing the address space (lives in `.bss`).
    static POOL: PageTablePool = PageTablePool::new();

    /// Backing storage for the EL0 stack. `align(4096)` so each page maps
    /// to a valid frame; `USER_STACK_PAGES` pages.
    #[repr(C, align(4096))]
    struct UserStack([u8; PAGE_SIZE * USER_STACK_PAGES]);

    static mut USER_STACK: UserStack = UserStack([0; PAGE_SIZE * USER_STACK_PAGES]);

    /// Set once the round-trip has been driven so a re-entry can never
    /// re-run the test logic (fail closed).
    static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

    fn note(id: EventId, level: Level, message: &'static str) {
        log(
            &SERIAL_SINK,
            &Event {
                level,
                id,
                message,
                fields: &[],
            },
        );
    }

    /// Forward to the shared aarch64 panic bridge (parks the CPU; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn tairix_abi_sys_syscall_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// The syscall dispatch callback installed for the round-trip.
    ///
    /// Reached from the EL1 vector trampoline after the EL0 stub executed
    /// `svc`. It asserts the marshalled `(number, args)` match what
    /// `tairix_sys_cap_query(EXPECTED_CAP)` should have placed in the
    /// registers, then reports PASS. It never returns to the caller (it
    /// diverges through `qemu_exit`): returning would `eret` back to EL0.
    ///
    /// The signature matches `syscall_entry::SyscallDispatchFn`.
    extern "C" fn record_and_exit(number: u64, args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64 {
        // SAFETY: the handler built the `[u64; SYSCALL_MAX_ARGS]` array
        // and passes a pointer valid for the duration of this call
        // (`syscall_entry` contract).
        let args = unsafe { *args_ptr };

        let expected_number = u64::from(SyscallNumber::CAP_QUERY.as_u16());
        if number != expected_number {
            note(
                TEST_FAIL,
                Level::Error,
                "dispatched the wrong syscall number",
            );
            qemu_exit::exit_failure(FAIL_WRONG_NUMBER);
        }

        let expected_arg0 = u64::from(EXPECTED_CAP.as_u16());
        if args[0] != expected_arg0 || args[1..] != [0, 0, 0, 0, 0] {
            note(
                TEST_FAIL,
                Level::Error,
                "dispatched the wrong argument vector",
            );
            qemu_exit::exit_failure(FAIL_WRONG_ARGS);
        }

        qemu_exit::exit_success();
    }

    /// Log a setup failure, report it to QEMU, and never return.
    fn fail_setup(what: &'static str) -> ! {
        note(TEST_FAIL, Level::Error, what);
        qemu_exit::exit_failure(FAIL_SETUP);
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        if TEST_DRIVEN
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // A second entry can only mean a kernel bug; fail closed.
            fail_setup("kernel_main re-entered");
        }

        note(
            TEST_START,
            Level::Info,
            "aarch64 abi-sys round-trip: building the EL0 context",
        );

        let page = PAGE_SIZE as u64;

        let mut space = AddressSpace::new_identity_gigapages(&POOL, IDENTITY_GIB)
            .unwrap_or_else(|| fail_setup("identity map"));

        // ---- Alias the stub's page(s) into EL0 (read + execute). ----
        let func = tairix_abi_sys::sys_cap_query as *const () as u64;
        let func_page = func & !(page - 1);
        // Map the stub's page plus the following one: the stub is far
        // smaller than a page, but mapping two pages is cheap insurance
        // against it straddling a page boundary.
        for i in 0..2u64 {
            space
                .map_4k_with_attrs(
                    &POOL,
                    USER_CODE_VA + i * page,
                    func_page + i * page,
                    el0_code_leaf_attrs(),
                )
                .unwrap_or_else(|| fail_setup("code alias"));
        }
        let user_entry = USER_CODE_VA | (func & (page - 1));

        // ---- Map the EL0 stack (read + write). ----
        let stack_phys = core::ptr::addr_of!(USER_STACK) as u64;
        for i in 0..USER_STACK_PAGES as u64 {
            space
                .map_4k_with_attrs(
                    &POOL,
                    USER_STACK_VA + i * page,
                    stack_phys + i * page,
                    el0_data_leaf_attrs(),
                )
                .unwrap_or_else(|| fail_setup("stack map"));
        }
        let user_sp = USER_STACK_VA + (USER_STACK_PAGES as u64) * page;

        // ---- Install the dispatch callback and the EL1 vector table. ----
        syscall_entry::set_dispatch_callback(record_and_exit);
        // SAFETY: called once on the boot CPU before any exception fires.
        unsafe { exceptions::init_vectors() };

        // SAFETY: the identity space maps the current `pc`, the stack, and
        // the device MMIO, so enabling the MMU is sound.
        unsafe { space.switch() };

        note(
            TEST_START,
            Level::Info,
            "dropping to EL0 to issue tairix_sys_cap_query",
        );

        // ---- Drop to EL0 and issue the stub via the Arch HAL. ----
        // SAFETY: `user_entry` aliases the EL0-executable stub page and
        // `user_sp` tops the EL0 stack, both mapped above; the dispatch
        // callback and vector table are installed. The `eret` sequence is
        // the one HAL definition (`tairix_arch_aarch64::userentry`).
        unsafe {
            UserMode::new().enter_user(UserEntry::new(
                user_entry,
                user_sp,
                u64::from(EXPECTED_CAP.as_u16()),
                // This vertical's program uses no thread-local storage.
                0,
            ))
        }

        // `enter_user` diverges via `eret`; this point is reached only if
        // the `svc` resumed in EL0 and the stub returned to a caller that
        // fell through — neither can happen here, so it is a closed
        // failure.
        #[allow(unreachable_code)]
        {
            note(TEST_FAIL, Level::Error, "svc resumed in EL0 unexpectedly");
            qemu_exit::exit_failure(FAIL_SVC_RETURNED);
        }
    }
}

// --- Stub when the test-hooks feature is off ----------------------
//
// The test body only compiles when `feature = "test-hooks"` is on.
// Disabling it leaves the bin as a no-op so a layout sanity check
// (`cargo build --no-default-features`) still builds
// (a disabled test must compile cleanly).
#[cfg(all(itest_aarch64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_dtb: u64) -> ! {
    loop {
        // SAFETY: `wfe` is a well-defined parked-CPU hint on aarch64. Looping defends against spurious wake-ups.
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(all(itest_aarch64, not(feature = "test-hooks")))]
#[panic_handler]
fn tairix_abi_sys_syscall_qemu_aarch64_panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        // SAFETY: same as above.
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
