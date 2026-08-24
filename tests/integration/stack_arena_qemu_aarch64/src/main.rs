//! `plans/PI.md` guard-page fault-form (stage G2) QEMU integration test:
//! the boot-time kthread-stack guard arena is laid out at 4 KiB
//! granularity so a guard page in it can be unmapped — raising a
//! synchronous hardware fault — **without** ever shattering the 2 MiB
//! block the running CPU executes on.
//!
//! ## Why this exists
//!
//! G1 proved the page-table block-split primitive
//! (`tairix_arch_aarch64::paging::AddressSpace::split_block`) on a
//! single page. G2 is the boot-map step that gives kthread kernel stacks a
//! dedicated arena, re-expressed at 4 KiB granularity up-front, so the
//! eventual guard-page unmap (stage G3) never has to break-before-make the
//! coarse block the CPU is currently running on or stacked in. This
//! vertical proves the property end to end on the `virt` board: the arena
//! is its own 2 MiB-aligned block, distinct from the block holding the
//! running code and stack, and unmapping a guard page inside it faults
//! while the running stack keeps working.
//!
//! ## What this test asserts
//!
//! 1. Build a stage-1 `AddressSpace` identity-mapping the low 2 GiB. The
//!    `ARENA` static is 2 MiB-aligned and exactly 2 MiB, so it occupies a
//!    whole L2 block of its own — the running code and boot stack live in
//!    *other* 2 MiB blocks of the same gigapage.
//! 2. `AddressSpace::prepare_guard_arena` re-expresses the arena's block
//!    at 4 KiB granularity, preserving every mapping. (Splitting the 1 GiB
//!    gigapage to 2 MiB blocks only *adds* table levels; only the arena's
//!    block becomes 4 KiB pages.)
//! 3. Activate the space (enabling the MMU). The running code/stack block
//!    stayed a coarse block and is still mapped, so execution continues.
//! 4. Write a sentinel through a guard page in the arena and read it back:
//!    the split preserved the arena mapping under the live MMU.
//! 5. `unmap(guard_va)` + `flush_page(guard_va)`: tear down exactly that
//!    one arena page through the Arch HAL.
//! 6. Prove the running stack still works (a stack-heavy scribble that
//!    would itself fault first if the running block had been broken) and
//!    that a *neighbouring* arena page is still mapped.
//! 7. Read `guard_va`: the MMU raises a data abort; the handler confirms it
//!    is an abort on exactly `guard_va` and reports PASS via the ARM
//!    semihosting finisher. A regression that left the page mapped reads it
//!    without faulting and reports FAILURE explicitly.
//!
//! ## How it differs from a production kernel
//!
//! It links only the `tairix-arch-aarch64` port and supplies its own
//! `kernel_main`. The QEMU-exit shortcut lives in this dedicated bin,
//! never behind a Cargo feature on the arch crate (fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;

    use tairix_arch_aarch64::paging::{AddressSpace, PageTablePool, BLOCK_2MIB, PAGE_SIZE};
    use tairix_arch_aarch64::{exceptions, fault, handle_panic_via_serial, qemu_exit, SERIAL_SINK};
    use tairix_arch_api::mmu::AddressSpace as _;
    use tairix_arch_api::tlb::TlbShootdown as _;
    use tairix_itest_finisher::fail_point;
    use tairix_log::{log, Event, EventId, Field, Level};

    /// Number of GiB the space identity-maps (device MMIO + RAM). The
    /// kernel image, stack, and `ARENA` all live in the Normal RAM
    /// gigapage (GiB 1).
    const IDENTITY_GIB: usize = 2;

    /// The sentinel written through an arena guard page after the split,
    /// to prove the split preserved the mapping before the page is torn
    /// down.
    const SENTINEL: u8 = 0x5A;

    /// Stable audit-event ids for the QEMU transcript.
    const SA_TEST_START: EventId = EventId(4303);
    const SA_TEST_PASS: EventId = EventId(4304);
    const SA_TEST_FAIL: EventId = EventId(4305);
    /// Failure finisher codes, distinct per failure site.
    const FAIL_NO_FAULT: NonZeroU16 = fail_point!(2);
    const FAIL_UNEXPECTED_FAULT: NonZeroU16 = fail_point!(3);
    const FAIL_SETUP: NonZeroU16 = fail_point!(4);

    /// Page-table pool backing the address space (lives in `.bss`).
    static POOL: PageTablePool = PageTablePool::new();

    /// The kthread-stack guard arena: a 2 MiB-aligned, 2 MiB region. Its
    /// alignment and size force it to occupy a whole L2 block of its own,
    /// so re-expressing it at 4 KiB granularity (and later unmapping one of
    /// its pages) never disturbs the 2 MiB block that holds the running
    /// code or boot stack. Its physical address is its identity-mapped
    /// virtual address.
    #[repr(C, align(0x20_0000))]
    struct Arena([u8; BLOCK_2MIB as usize]);
    static mut ARENA: Arena = Arena([0; BLOCK_2MIB as usize]);

    /// Offset of the chosen guard page within the arena (page 8 — well
    /// inside, with mapped neighbours on both sides).
    const GUARD_OFFSET: u64 = 8 * PAGE_SIZE as u64;

    /// Virtual (== physical) address of the guard page under test.
    fn guard_va() -> u64 {
        core::ptr::addr_of!(ARENA) as u64 + GUARD_OFFSET
    }

    /// The fault handler: confirm the trap is a data/instruction abort on
    /// exactly the (now-unmapped) arena guard page, then report PASS.
    /// Anything else is a FAILURE. Never returns.
    extern "C" fn on_fault(esr: u64, far: u64, _elr: u64) -> ! {
        if fault::is_abort(esr) && far == guard_va() {
            log(
                &SERIAL_SINK,
                &Event {
                    level: Level::Info,
                    id: SA_TEST_PASS,
                    message: "aarch64 stack-arena test: faulted on the unmapped arena guard page",
                    fields: &[],
                },
            );
            qemu_exit::exit_success();
        }
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: SA_TEST_FAIL,
                message: "aarch64 stack-arena test: unexpected fault",
                fields: &[],
            },
        );
        qemu_exit::exit_failure(FAIL_UNEXPECTED_FAULT);
    }

    /// Forward to the shared aarch64 panic bridge (parks the CPU; the run
    /// then times out and the harness reports the failure).
    #[panic_handler]
    fn tairix_stack_arena_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: SA_TEST_START,
                message: "aarch64 stack-arena test: preparing a guard arena and unmapping one page",
                fields: &[],
            },
        );

        let arena_base = core::ptr::addr_of!(ARENA) as u64;
        let guard = guard_va();

        // Build the identity space. The arena is mapped by a coarse block.
        let mut space = AddressSpace::new_identity_gigapages(&POOL, IDENTITY_GIB)
            .unwrap_or_else(|| fail("identity map"));

        // Re-express the arena's block at 4 KiB granularity. Done while the
        // space is inactive; this never breaks the running region's
        // mapping (it only adds table levels reproducing the translation).
        space
            .prepare_guard_arena(arena_base, BLOCK_2MIB)
            .unwrap_or_else(|_| fail("prepare_guard_arena"));

        // Install the vector table and fault handler before enabling the
        // MMU so the deliberate abort is routed to `on_fault`.
        fault::set_fault_handler(on_fault).unwrap_or_else(|_| fail("set_fault_handler"));
        // SAFETY: called once on the boot CPU before any fault can fire.
        unsafe {
            exceptions::init_vectors();
        }

        // Switch to the space (enables the MMU). The running code/stack
        // block stayed a coarse block and is identity-mapped, so execution
        // continues.
        // SAFETY: the space identity-maps `pc`, `sp`, and MMIO per
        // `new_identity_gigapages`; preparing the arena only re-expressed
        // its own block at finer granularity.
        unsafe {
            space.activate();
        }

        // Prove the split preserved the arena guard page's mapping under
        // the live MMU: write a sentinel and read it back.
        // SAFETY: `guard` maps a live page-aligned slot inside `ARENA`,
        // mapped RW; the access is well-defined while the page is mapped.
        unsafe {
            core::ptr::write_volatile(guard as *mut u8, SENTINEL);
        }
        if unsafe { core::ptr::read_volatile(guard as *const u8) } != SENTINEL {
            fail("prepare did not preserve the arena mapping");
        }

        // Tear the single guard page down through the Arch HAL and flush
        // its stale TLB entry — exactly the production guard-page mechanism.
        space
            .unmap(guard)
            .unwrap_or_else(|_| fail("unmap arena guard page"));
        space.flush_page(guard);

        // The running stack lives in a *different* 2 MiB block, which was
        // never shattered: a stack-heavy scribble must still work (it would
        // fault here, before the guard read, if the running block had been
        // broken). The neighbouring arena page must also still be mapped.
        exercise_running_stack();
        let neighbour = guard + PAGE_SIZE as u64;
        // SAFETY: `neighbour` is a still-mapped arena page (only `guard`
        // was torn down); a single-byte read is well-defined.
        let _ = unsafe { core::ptr::read_volatile(neighbour as *const u8) };

        // Read the now-unmapped guard page. This must raise a translation
        // fault → `on_fault` (which exits PASS).
        // SAFETY: the access is *expected* to fault; if the MMU wrongly
        // permitted it the read is still of a valid pointer-sized region we
        // then report as a FAILURE below.
        let observed = unsafe { core::ptr::read_volatile(guard as *const u8) };

        // Reaching here means the unmapped page was read without a fault —
        // the guard FAILED. Reference `observed` so the read is not elided.
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: SA_TEST_FAIL,
                message: "aarch64 stack-arena test: read the unmapped guard page (no fault)",
                fields: &[Field {
                    key: "observed",
                    value: tairix_log::FieldValue::Str(if observed == SENTINEL {
                        "sentinel"
                    } else {
                        "other"
                    }),
                }],
            },
        );
        qemu_exit::exit_failure(FAIL_NO_FAULT);
    }

    /// Touch a chunk of the running kernel stack and read it back, proving
    /// the block the CPU is stacked in stayed mapped after the arena page
    /// was unmapped. `volatile` accesses keep the writes/reads from being
    /// optimised away.
    #[inline(never)]
    fn exercise_running_stack() {
        let mut scratch = [0u8; 1024];
        for (i, slot) in scratch.iter_mut().enumerate() {
            // SAFETY: `slot` points at a live local; the volatile write is
            // well-defined and prevents the scribble being elided.
            unsafe { core::ptr::write_volatile(slot, (i & 0xFF) as u8) };
        }
        let mut sum: u32 = 0;
        for slot in &scratch {
            sum = sum.wrapping_add(unsafe { core::ptr::read_volatile(slot) } as u32);
        }
        if sum == 0 {
            fail("running stack scribble was elided");
        }
    }

    /// Log a setup failure and report it to QEMU. Never returns.
    fn fail(what: &str) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Error,
                id: SA_TEST_FAIL,
                message: "aarch64 stack-arena test: setup failed",
                fields: &[Field {
                    key: "stage",
                    value: tairix_log::FieldValue::Str(what),
                }],
            },
        );
        qemu_exit::exit_failure(FAIL_SETUP);
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
