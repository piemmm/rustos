//! CCOMPAT stage CC3 QEMU exercise: the x86_64 ring-3 round-trip for the
//! Arch HAL "enter user mode" primitive (`rustos_arch_api::EnterUser`,
//! `kernel/arch/x86_64/src/userentry.rs`, `AGENTS.md` §17.2).
//!
//! ## What this test asserts
//!
//! The sibling `rustos-test-abi-sys-syscall-qemu` issues the `abi-sys`
//! stub from **ring 0** — the x86_64 `syscall` instruction traps into the
//! kernel entry path identically from any privilege level, so that test
//! never crosses a privilege boundary. This test does: it drops the CPU
//! to **ring 3** through the HAL `iretq` primitive and issues the stub
//! from there, so the `iretq` transition and a genuine ring-3 → ring-0
//! `syscall` are exercised end-to-end (the x86_64 analogue of the
//! riscv64/aarch64 CC2 round-trips, which already reach the transition
//! through the same HAL handle).
//!
//! ## How it asserts it
//!
//! The production `rustos-kernel` boot pipeline runs until
//! `AuditEvent::BootCompleted` (`EventId(4004)`). By that point the GDT
//! carries the ring-3 code/data descriptors, the TSS is installed, and
//! `syscall`/`IA32_LSTAR` entry is enabled on the BSP
//! (`init_local_syscalls`). The audit Sink that observes `BootCompleted`
//! then, using the Stage-3 paging primitives
//! (`rustos_arch_x86_64::paging`):
//!
//! 1. Builds one `paging::AddressSpace` that identity-maps the low
//!    32 MiB **and** mirrors the higher-half kernel window, so the kernel
//!    code/stack/data, the per-CPU `swapgs` TLS, the dispatch callback,
//!    and this stub's page stay reachable after the CR3 switch.
//! 2. Aliases the page(s) holding the `ros_sys_cap_query` stub at a
//!    ring-3 virtual address (`USER_CODE_VA`) **user-accessible,
//!    executable, not writable** (`map_4k_user(writable = false)` — W^X,
//!    `AGENTS.md` §19.2), and maps a USER read/write stack at
//!    `USER_STACK_VA`. The kernel's own mappings carry no USER bit, so
//!    ring 3 can reach *only* the aliased stub and its stack — the §4
//!    isolation contract.
//! 3. Overrides the dispatch callback with `record_and_exit`, switches
//!    CR3 to the new space, and `iretq`s to ring 3 at the stub through
//!    `UserMode::new().enter_user(...)`.
//!
//! The stub's real `syscall` (`lib/abi-sys/src/trap.rs`) raises into the
//! kernel's `IA32_LSTAR` entry stub, which `swapgs`es, pivots onto the
//! per-CPU kernel stack, rebuilds the canonical `[u64; SYSCALL_MAX_ARGS]`
//! array, and calls the installed callback. `record_and_exit` therefore
//! observes the register marshalling end-to-end **and** proves the
//! ring-3 entry succeeded (the `syscall` could only originate from the
//! ring-3 stub). It asserts the dispatched number is
//! `SyscallNumber::CAP_QUERY` and that argument 0 is the capability id the
//! stub was handed (the rest zero), then flips `qemu_exit::exit_success`.
//! Any mismatch — or the round-trip never reaching the callback — flips
//! `qemu_exit::exit_failure`.
//!
//! ## `test-hooks` Cargo feature
//!
//! The test body only compiles under `#[cfg(feature = "test-hooks")]`.
//! The feature is on by default for this crate; release builds that
//! enable it are rejected by the `compile_error!` guard below
//! (AGENTS.md §1 — no hacks; §5.4.5 — fail closed), mirroring
//! `rustos-test-abi-sys-syscall-qemu`.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// AGENTS.md §1 — test affordances must never reach a release binary.
#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-enter-user-qemu-x86_64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(all(itest_x86_64, feature = "test-hooks"))]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, Ordering};

    use rustos_abi::{CapabilityId, SyscallNumber, SYSCALL_MAX_ARGS};
    use rustos_arch_api::{EnterUser, UserEntry};
    use rustos_arch_x86_64::userentry::UserMode;
    use rustos_arch_x86_64::{paging, qemu_exit, syscall_entry};
    use rustos_kernel::kalloc::{Heap, HEAP_BYTES};
    use rustos_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use rustos_log::{Event, EventId, Sink};

    /// Static heap for the bump allocator (per the production bin).
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: as for the production bin's `ALLOCATOR` — the page-aligned
    /// `HEAP` static outlives the binary and the allocator is its only
    /// consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// `EventId` emitted when every boot init phase completed.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// Capability id passed to `ros_sys_cap_query` and expected in
    /// argument 0. Any well-known [`CapabilityId`] works — the test
    /// asserts the stub's *marshalling*, not the kernel's grant decision.
    const EXPECTED_CAP: CapabilityId = CapabilityId::TIME_SET;

    /// Ring-3 virtual address the stub code is aliased at. 64 GiB — far
    /// above the 32 MiB low identity window
    /// ([`paging::AddressSpace::new_identity_first_32mib`]) — so the alias
    /// lands on freshly-walked tables under the shared PML4[0]/PDPT, not
    /// on an identity huge-page leaf. Page aligned and canonical.
    const USER_CODE_VA: u64 = 0x10_0000_0000;

    /// Ring-3 virtual address the stack is mapped at (4 MiB above the code
    /// alias so the two never share a page-table leaf). Page aligned.
    const USER_STACK_VA: u64 = 0x10_0040_0000;

    /// User stack size in 4 KiB pages (16 KiB — ample for the stub
    /// prologue; the kernel `syscall` entry pivots onto its own per-CPU
    /// stack, so the ring-3 stack is only the stub's).
    const USER_STACK_PAGES: u64 = 4;

    /// Page-table pool backing the user address space (lives in `.bss`).
    static PAGE_TABLE_POOL: paging::PageTablePool = paging::PageTablePool::new();

    /// Backing storage for the ring-3 stack. `align(4096)` so each page
    /// maps to a valid frame.
    #[repr(C, align(4096))]
    struct UserStack([u8; paging::PAGE_SIZE * USER_STACK_PAGES as usize]);

    static mut USER_STACK: UserStack =
        UserStack([0; paging::PAGE_SIZE * USER_STACK_PAGES as usize]);

    /// Set once the round-trip has been driven so a stray duplicate
    /// `BootCompleted` can never re-enter the test logic
    /// (`AGENTS.md` §5.4.5 — fail closed).
    static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

    /// The syscall dispatch callback installed for the round-trip.
    ///
    /// Reached from the kernel's `IA32_LSTAR` entry stub after the
    /// ring-3 stub executed `syscall`. Reaching it at all proves the
    /// `iretq` ring-3 entry succeeded. It asserts the marshalled
    /// `(number, args)` then exits QEMU; it never returns (a `sysretq`
    /// here would re-enter ring 3, but the test is complete).
    extern "C" fn record_and_exit(number: u64, args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64 {
        // SAFETY: the entry stub built the `[u64; SYSCALL_MAX_ARGS]` array
        // on the kernel stack and passes a pointer valid for the duration
        // of this call (`syscall_entry` contract).
        let args = unsafe { *args_ptr };

        let expected_number = u64::from(SyscallNumber::CAP_QUERY.as_u16());
        let expected_arg0 = u64::from(EXPECTED_CAP.as_u16());
        let args_ok = args[0] == expected_arg0 && args[1..] == [0, 0, 0, 0, 0];

        if number == expected_number && args_ok {
            qemu_exit::exit_success();
        }
        qemu_exit::exit_failure();
    }

    /// Outer audit sink: replays every event to serial (so the QEMU
    /// transcript captures the boot timeline) and, on the single
    /// [`BOOT_COMPLETED_EVENT_ID`], drives [`run_round_trip`].
    struct BootCompletedSink;

    impl Sink for BootCompletedSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);

            if event.id == BOOT_COMPLETED_EVENT_ID
                && TEST_DRIVEN
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                run_round_trip();
            }
        }
    }

    static AUDIT_SINK: BootCompletedSink = BootCompletedSink;

    /// Build the ring-3 address space, install the dispatch callback, and
    /// drop to ring 3 to issue the stub. Never returns.
    fn run_round_trip() -> ! {
        let page = paging::PAGE_SIZE as u64;

        let Some(mut space) = paging::AddressSpace::new_identity_first_32mib(&PAGE_TABLE_POOL)
        else {
            qemu_exit::exit_failure();
        };

        // ---- Alias the stub's page(s) into ring 3 (USER|R|X). ----
        let func_va = rustos_abi_sys::sys_cap_query as *const () as u64;
        let func_phys = func_va - paging::KERNEL_VMA_BASE;
        let func_page = func_phys & !(page - 1);
        // Map the stub's page plus the following one as cheap insurance
        // against the stub straddling a page boundary. `writable = false`
        // keeps the executable alias non-writable (W^X, §19.2).
        for i in 0..2u64 {
            if space
                .map_4k_user(
                    &PAGE_TABLE_POOL,
                    USER_CODE_VA + i * page,
                    func_page + i * page,
                    false,
                )
                .is_none()
            {
                qemu_exit::exit_failure();
            }
        }
        let user_entry = USER_CODE_VA | (func_va & (page - 1));

        // ---- Map the ring-3 stack (USER|R|W). ----
        let stack_phys = core::ptr::addr_of!(USER_STACK) as u64 - paging::KERNEL_VMA_BASE;
        for i in 0..USER_STACK_PAGES {
            if space
                .map_4k_user(
                    &PAGE_TABLE_POOL,
                    USER_STACK_VA + i * page,
                    stack_phys + i * page,
                    true,
                )
                .is_none()
            {
                qemu_exit::exit_failure();
            }
        }
        // Top of the stack, 16-aligned, then -8 so the stub is entered
        // with `rsp ≡ 8 (mod 16)` — the System V AMD64 state just after a
        // `call`, which the compiler's prologue assumes.
        let user_sp = (USER_STACK_VA + USER_STACK_PAGES * page) - 8;

        syscall_entry::set_dispatch_callback(record_and_exit);

        // Identity-map the architectural LAPIC MMIO page (supervisor-only).
        // The production boot now arms ring-3 preemption (P-1c): a periodic
        // LAPIC-timer IRQ is taken while the stub runs in ring 3 (under this
        // CR3), and its ISR reads the LAPIC ID register and writes EOI at
        // `LAPIC_BASE_PHYS`. Without this mapping that kernel-mode MMIO access
        // would page-fault under the minimal user CR3 (`AGENTS.md` §2.17) — the
        // same page the production / timeshare spaces map. The preempt callback
        // then no-ops here (no user kthread is published), so preemption stays
        // transparent to this round-trip.
        if space
            .map_4k(
                &PAGE_TABLE_POOL,
                rustos_arch_x86_64::preempt::LAPIC_BASE_PHYS,
                rustos_arch_x86_64::preempt::LAPIC_BASE_PHYS,
                true,
            )
            .is_none()
        {
            qemu_exit::exit_failure();
        }

        // SAFETY: the new space maps the low 32 MiB, the higher-half kernel
        // window, and the LAPIC MMIO page, so the currently executing RIP, the
        // current stack, the per-CPU `swapgs` TLS, `record_and_exit`, and the
        // timer ISR's LAPIC access all stay mapped across the switch.
        unsafe { space.switch() };

        // SAFETY: `user_entry` aliases the executable USER|R|X stub page
        // and `user_sp` tops the USER|R|W stack, both mapped above; the
        // dispatch callback is installed and the GDT user selectors / TSS
        // / `syscall` entry were installed during boot. The `iretq`
        // sequence is the one HAL definition
        // (`rustos_arch_x86_64::userentry`).
        unsafe {
            UserMode::new().enter_user(UserEntry::new(
                user_entry,
                user_sp,
                u64::from(EXPECTED_CAP.as_u16()),
            ))
        }
    }

    /// Forward to the shared bridge in `rustos_kernel`.
    #[panic_handler]
    fn rustos_test_enter_user_qemu_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(multiboot_info, &SERIAL_SINK, &AUDIT_SINK)
    }
}

// --- Stub when the test-hooks feature is off ----------------------
#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_multiboot_info: u64) -> ! {
    loop {
        // SAFETY: `cli; hlt` is a well-defined parked-CPU sequence on
        // x86_64 (`AGENTS.md` §2.9). Looping defends against spurious
        // wake-ups.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[panic_handler]
fn rustos_test_enter_user_qemu_panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        // SAFETY: same as above.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
