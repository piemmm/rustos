//! x86_64 `syscall` register-preservation regression vertical.
//!
//! ## What this test asserts
//!
//! The x86_64 user→kernel trap stub (`lib/abi-trap`) declares to the
//! compiler that a `syscall` clobbers only `rax` (the result), `rcx`
//! (saved RIP), and `r11` (saved RFLAGS). The kernel's `IA32_LSTAR`
//! entry stub (`kernel/arch/x86_64/src/syscall_entry.rs`) must therefore
//! hand every *other* register back to ring 3 exactly as the caller left
//! it. It once tore its on-stack argument array down with a bare stack
//! drop instead of popping the values back into
//! `rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9`, so after `sysretq` those registers
//! held kernel dispatch residue: a call-site-dependent miscompilation of
//! every syscall wrapper (the compiler legitimately re-used "unchanged"
//! registers, e.g. computing a buffer length from residue) **and** a
//! kernel-register information leak into ring 3. This test pins the
//! contract: after a real ring-3 `syscall` round-trip, the six argument
//! registers, the six callee-saved registers (`rbx`/`rbp`/`r12`–`r15`),
//! and the callback-returned `rax` are all exactly what they must be.
//!
//! ## How it asserts it
//!
//! A returning `syscall` can only be exercised from ring 3 (`sysretq`
//! always returns to CPL 3), so the test enters ring 3 exactly like the
//! sibling `tairix-test-enter-user-qemu-x86_64`: boot the production
//! `tairix-kernel` pipeline until `AuditEvent::BootCompleted`, build a
//! user address space with a USER|R|X alias of the probe's page(s) and a
//! USER|R|W stack, install a test dispatch callback, switch CR3, and
//! `iretq` to ring 3 at the probe. The probe (a naked-asm fragment — the
//! register loads/compares around a raw `syscall` cannot be expressed in
//! Rust, so this file uses one of the charter's permitted assembly
//! fragments):
//!
//! 1. Loads a distinct sentinel into each of `rdi`/`rsi`/`rdx`/`r10`/
//!    `r8`/`r9` (the syscall argument registers) and `rbx`/`rbp`/`r12`/
//!    `r13`/`r14`/`r15` (the callee-saved set).
//! 2. Issues `syscall` with `PROBE_NUMBER`. The installed callback
//!    checks the kernel-observed argument array carries exactly the six
//!    argument sentinels (the entry marshalling), then returns
//!    `PROBE_RETURN_SENTINEL`.
//! 3. Back in ring 3, xor-accumulates the difference between every one
//!    of the twelve sentinel registers (plus the returned `rax`) and its
//!    expected value into `r11` (architecturally clobbered by `syscall`,
//!    so free as an accumulator), and issues a second `syscall` with
//!    `REPORT_NUMBER` and `arg0 = 1` iff every register survived.
//!
//! The callback flips `qemu_exit::exit_success` on a clean verdict and
//! `qemu_exit::exit_failure` on any mismatch — before the entry-stub fix
//! this test fails (the argument registers come back as dispatch
//! residue); with it, it passes.
//!
//! ## `test-hooks` Cargo feature
//!
//! As for the sibling verticals, the test body only compiles under
//! `#[cfg(feature = "test-hooks")]`, and release builds that enable the
//! feature are rejected by the `compile_error!` guard below (fail
//! closed).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// Test affordances must never reach a release binary.
#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-syscall-regs-qemu-x86_64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds."
);

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(all(itest_x86_64, feature = "test-hooks"))]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, Ordering};

    use tairix_abi::SYSCALL_MAX_ARGS;
    use tairix_arch_api::{EnterUser, UserEntry};
    use tairix_arch_x86_64::userentry::UserMode;
    use tairix_arch_x86_64::{paging, qemu_exit, syscall_entry};
    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use tairix_log::{Event, EventId, Sink};

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

    /// Syscall number the probe's first (sentinel-carrying) `syscall`
    /// uses. The test overrides the dispatch callback, so the value never
    /// reaches the production dispatcher; it only needs to be distinct
    /// from [`REPORT_NUMBER`].
    const PROBE_NUMBER: u64 = 0x5150;

    /// Syscall number of the probe's verdict report (`arg0 = 1` iff every
    /// register survived the first round-trip).
    const REPORT_NUMBER: u64 = 0x5251;

    /// Value the callback returns for the probe `syscall`; the probe
    /// verifies it arrives in `rax` unchanged through `sysretq`.
    const PROBE_RETURN_SENTINEL: u64 = 0x0EC0_FFEE_5CA1_AB1E;

    /// Sentinels loaded into the six syscall argument registers
    /// (`rdi`/`rsi`/`rdx`/`r10`/`r8`/`r9`) before the probe `syscall`.
    /// The kernel must marshal exactly these into the dispatch argument
    /// array *and* hand exactly these back to ring 3.
    const ARG_SENTINELS: [u64; 6] = [
        0xA5A5_0001_1111_1101,
        0xA5A5_0002_2222_2202,
        0xA5A5_0003_3333_3303,
        0xA5A5_0004_4444_4404,
        0xA5A5_0005_5555_5505,
        0xA5A5_0006_6666_6606,
    ];

    /// Sentinels loaded into the callee-saved registers
    /// (`rbx`/`rbp`/`r12`/`r13`/`r14`/`r15`); the System V contract the
    /// kernel-side Rust dispatch already upholds, pinned here end to end.
    const SAVED_SENTINELS: [u64; 6] = [
        0x5EED_0007_7777_7707,
        0x5EED_0008_8888_8808,
        0x5EED_0009_9999_9909,
        0x5EED_000A_AAAA_AA0A,
        0x5EED_000B_BBBB_BB0B,
        0x5EED_000C_CCCC_CC0C,
    ];

    /// Ring-3 virtual address the probe code is aliased at (as for the
    /// sibling enter-user vertical: far above the 32 MiB low identity
    /// window so the alias lands on freshly-walked tables).
    const USER_CODE_VA: u64 = 0x10_0000_0000;

    /// Ring-3 virtual address the stack is mapped at (4 MiB above the
    /// code alias so the two never share a page-table leaf).
    const USER_STACK_VA: u64 = 0x10_0040_0000;

    /// User stack size in 4 KiB pages (16 KiB — ample; the probe itself
    /// never touches its stack).
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
    /// `BootCompleted` can never re-enter the test logic (fail closed).
    static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

    /// The ring-3 probe: sentinels in, `syscall`, verify, report.
    ///
    /// Naked assembly because the test's whole subject is raw register
    /// state around the `syscall` instruction, which compiled Rust cannot
    /// pin (a charter-permitted assembly fragment; see the module docs).
    /// The fragment is position-independent — immediates and register
    /// operations only — so the kernel-half function body can be aliased at
    /// [`USER_CODE_VA`] and executed there.
    ///
    /// `r11` is the verdict accumulator and `rcx` the scratch register:
    /// both are architecturally clobbered by `syscall` itself (saved
    /// RFLAGS/RIP), so using them asserts nothing weaker.
    ///
    /// # Safety
    ///
    /// Only entered through the ring-3 alias built by [`run_round_trip`];
    /// it never returns (the report `syscall`'s callback exits QEMU, and
    /// a `ud2` backstop follows it).
    #[unsafe(naked)]
    #[no_mangle]
    unsafe extern "C" fn ring3_reg_probe() -> ! {
        core::arch::naked_asm!(
            // 1. Sentinels into the six argument registers…
            "movabsq ${a0}, %rdi",
            "movabsq ${a1}, %rsi",
            "movabsq ${a2}, %rdx",
            "movabsq ${a3}, %r10",
            "movabsq ${a4}, %r8",
            "movabsq ${a5}, %r9",
            // …and the six callee-saved registers.
            "movabsq ${s0}, %rbx",
            "movabsq ${s1}, %rbp",
            "movabsq ${s2}, %r12",
            "movabsq ${s3}, %r13",
            "movabsq ${s4}, %r14",
            "movabsq ${s5}, %r15",
            // 2. The probe round-trip.
            "movq ${probe}, %rax",
            "syscall",
            // 3. Verdict: r11 accumulates the xor-difference of every
            //    register against its expected value (0 = all survived).
            "xorq %r11, %r11",
            "movabsq ${a0}, %rcx",
            "xorq %rdi, %rcx",
            "orq %rcx, %r11",
            "movabsq ${a1}, %rcx",
            "xorq %rsi, %rcx",
            "orq %rcx, %r11",
            "movabsq ${a2}, %rcx",
            "xorq %rdx, %rcx",
            "orq %rcx, %r11",
            "movabsq ${a3}, %rcx",
            "xorq %r10, %rcx",
            "orq %rcx, %r11",
            "movabsq ${a4}, %rcx",
            "xorq %r8, %rcx",
            "orq %rcx, %r11",
            "movabsq ${a5}, %rcx",
            "xorq %r9, %rcx",
            "orq %rcx, %r11",
            "movabsq ${s0}, %rcx",
            "xorq %rbx, %rcx",
            "orq %rcx, %r11",
            "movabsq ${s1}, %rcx",
            "xorq %rbp, %rcx",
            "orq %rcx, %r11",
            "movabsq ${s2}, %rcx",
            "xorq %r12, %rcx",
            "orq %rcx, %r11",
            "movabsq ${s3}, %rcx",
            "xorq %r13, %rcx",
            "orq %rcx, %r11",
            "movabsq ${s4}, %rcx",
            "xorq %r14, %rcx",
            "orq %rcx, %r11",
            "movabsq ${s5}, %rcx",
            "xorq %r15, %rcx",
            "orq %rcx, %r11",
            "movabsq ${ret}, %rcx",
            "xorq %rax, %rcx",
            "orq %rcx, %r11",
            // 4. Report: arg0 = 1 iff the accumulator is zero.
            "xorq %rdi, %rdi",
            "testq %r11, %r11",
            "sete %dil",
            "movq ${report}, %rax",
            "syscall",
            // The report callback never returns; trap if it somehow does.
            "ud2",
            a0 = const ARG_SENTINELS[0],
            a1 = const ARG_SENTINELS[1],
            a2 = const ARG_SENTINELS[2],
            a3 = const ARG_SENTINELS[3],
            a4 = const ARG_SENTINELS[4],
            a5 = const ARG_SENTINELS[5],
            s0 = const SAVED_SENTINELS[0],
            s1 = const SAVED_SENTINELS[1],
            s2 = const SAVED_SENTINELS[2],
            s3 = const SAVED_SENTINELS[3],
            s4 = const SAVED_SENTINELS[4],
            s5 = const SAVED_SENTINELS[5],
            probe = const PROBE_NUMBER,
            report = const REPORT_NUMBER,
            ret = const PROBE_RETURN_SENTINEL,
            options(att_syntax),
        )
    }

    /// The syscall dispatch callback installed for the round-trip.
    ///
    /// First call ([`PROBE_NUMBER`]): asserts the entry stub marshalled
    /// exactly the six argument sentinels, then returns
    /// [`PROBE_RETURN_SENTINEL`] so the probe resumes in ring 3 — the
    /// return path under test. Second call ([`REPORT_NUMBER`]): flips the
    /// QEMU exit on the probe's verdict. Anything else fails closed.
    extern "C" fn probe_dispatch(number: u64, args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64 {
        // SAFETY: the entry stub built the `[u64; SYSCALL_MAX_ARGS]` array
        // on the kernel stack and passes a pointer valid for the duration
        // of this call (`syscall_entry` contract).
        let args = unsafe { *args_ptr };

        if number == PROBE_NUMBER {
            if args == ARG_SENTINELS {
                return PROBE_RETURN_SENTINEL;
            }
            // The entry marshalling itself is broken — fail loud now
            // rather than reporting a confusing verdict later.
            qemu_exit::exit_failure();
        }
        if number == REPORT_NUMBER && args[0] == 1 {
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
    /// drop to ring 3 to run the probe. Never returns. (The same shape as
    /// the sibling enter-user vertical; see its docs for the mapping
    /// rationale.)
    fn run_round_trip() -> ! {
        let page = paging::PAGE_SIZE as u64;

        let Some(mut space) = paging::AddressSpace::new_identity_first_32mib(&PAGE_TABLE_POOL)
        else {
            qemu_exit::exit_failure();
        };

        // ---- Alias the probe's page(s) into ring 3 (USER|R|X). ----
        let func_va = ring3_reg_probe as *const () as u64;
        let func_phys = func_va - paging::KERNEL_VMA_BASE;
        let func_page = func_phys & !(page - 1);
        // The probe's page plus the following one as cheap insurance against
        // the fragment straddling a page boundary. `writable = false` keeps the
        // executable alias non-writable (W^X).
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
        // Top of the stack, 16-aligned, then -8 so the probe is entered
        // with `rsp ≡ 8 (mod 16)` — the System V AMD64 state just after a
        // `call`.
        let user_sp = (USER_STACK_VA + USER_STACK_PAGES * page) - 8;

        syscall_entry::set_dispatch_callback(probe_dispatch);

        // Identity-map the architectural LAPIC MMIO page (supervisor-only)
        // so the ring-3 preemption timer ISR keeps working under this CR3,
        // exactly as the sibling enter-user vertical maps it.
        if space
            .map_4k(
                &PAGE_TABLE_POOL,
                tairix_arch_x86_64::preempt::LAPIC_BASE_PHYS,
                tairix_arch_x86_64::preempt::LAPIC_BASE_PHYS,
                true,
            )
            .is_none()
        {
            qemu_exit::exit_failure();
        }

        // SAFETY: the new space maps the low 32 MiB, the higher-half kernel
        // window, and the LAPIC MMIO page, so the currently executing RIP,
        // the current stack, the per-CPU `swapgs` TLS, `probe_dispatch`, and
        // the timer ISR's LAPIC access all stay mapped across the switch.
        unsafe { space.switch() };

        // SAFETY: `user_entry` aliases the executable USER|R|X probe page
        // and `user_sp` tops the USER|R|W stack, both mapped above; the
        // dispatch callback is installed and the GDT user selectors / TSS
        // / `syscall` entry were installed during boot.
        unsafe { UserMode::new().enter_user(UserEntry::new(user_entry, user_sp, 0, 0)) }
    }

    /// Forward to the shared bridge in `tairix_kernel`.
    #[panic_handler]
    fn tairix_test_syscall_regs_qemu_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(
            multiboot_info,
            &SERIAL_SINK,
            &AUDIT_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Stub when the test-hooks feature is off ----------------------
#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_multiboot_info: u64) -> ! {
    loop {
        // SAFETY: `cli; hlt` is a well-defined parked-CPU sequence on
        // x86_64. Looping defends against spurious wake-ups.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[panic_handler]
fn tairix_test_syscall_regs_qemu_panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
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
