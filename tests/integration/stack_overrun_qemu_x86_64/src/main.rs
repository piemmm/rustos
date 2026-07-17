//! `plans/PI.md` guard-page fault-form (stage G3c, x86_64) QEMU
//! integration test: the **production** fault-form for a kthread kernel
//! stack — a live, scheduled kthread running on an arena-backed stack
//! whose one-page guard is *unmapped* takes a **synchronous, supervisor-
//! mode not-present page fault** the instant it overruns into that guard
//! page, rather than the deferred next-reschedule canary detection a
//! heap-backed `tairix_kernel_core::BoxStack` falls back to. The x86_64
//! sibling of `tests/integration/stack_overrun_qemu_aarch64`.
//!
//! ## Why this exists
//!
//! G1 proved the x86_64 four-level huge-page block-split primitive, and
//! G2 proved a guard arena can have a single page unmapped (faulting on a
//! *direct* access) without shattering the block the CPU runs on
//! (`stack_guard_qemu_x86_64`). What was still unproven on x86_64 is the
//! payoff: that an *overrunning kthread* — a task whose execution runs off
//! the bottom of its usable kernel stack — faults **synchronously in
//! hardware** under the live scheduler, instead of being caught only at
//! the next reschedule by `tairix_kernel_core::KernelStack::check_guard`
//! (the software-canary fallback the heap-backed `BoxStack` uses). This
//! vertical closes that gap on x86_64.
//!
//! ## What this test asserts
//!
//! Unlike the self-contained aarch64 vertical, x86_64 long-mode bring-up
//! (GDT, TSS, the dedicated error-code-aware `#PF` entry, the bump heap)
//! is the production boot pipeline's job, so this test boots the real
//! `tairix-kernel` pipeline and, on `AuditEvent::BootCompleted`:
//!
//! 1. Builds a `paging::AddressSpace` identity-mapping the low 4 GiB (so
//!    the running RIP / stack / per-CPU TLS and the guard arena's
//!    low-identity physical alias all stay reachable across the CR3 switch)
//!    plus the higher-half kernel window, and activates it (CR3).
//! 2. Re-expresses a 2 MiB-aligned, 2 MiB guard arena (`ARENA`) at 4 KiB
//!    granularity through the Arch HAL (`AddressSpace::prepare_guard_arena`,
//!    G2) so a single guard page in it can be torn down.
//! 3. Carves one kthread stack region out of the arena, laid out exactly
//!    like `tairix_kernel_core::BoxStack` / the production `ArenaStack`:
//!    `[guard page | usable stack]`, the guard immediately *below* the
//!    usable region so a downward overrun crosses it first.
//! 4. `unmap`s the guard page through the Arch HAL + `flush_page`s it — the
//!    production guard-page mechanism (G3b-2). The usable stack above it
//!    stays mapped.
//! 5. Builds the live `tairix_kernel_sched_eevdf::Scheduler` over
//!    `X86_64Arch` and admits a kthread on that stack via
//!    `spawn_kthread_with_stack` — the production runtime path, not a bare
//!    function call.
//! 6. The kthread body overruns its stack: it writes the highest byte of
//!    the guard region (the first byte a contiguous downward stack overrun
//!    crosses). Because that page is unmapped, the access raises a
//!    synchronous, supervisor-mode not-present `#PF` while the kthread is
//!    *running* (not at its next yield).
//! 7. The `tairix_arch_x86_64::fault` observer confirms the trap is a
//!    supervisor not-present fault on exactly the guard page and reports
//!    PASS. A regression that left the page mapped lets the body return
//!    cleanly; the cooperative `step` loop then drains the task and the
//!    test reports FAILURE explicitly (the guard was not enforced) rather
//!    than passing.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-stack-overrun-qemu-x86_64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

#[cfg(all(itest_x86_64, feature = "test-hooks"))]
mod kernel;

// --- Stub when the test-hooks feature is off ----------------------
#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_multiboot_info: u64) -> ! {
    loop {
        // SAFETY: `cli; hlt` is a well-defined parked-CPU sequence on x86_64. Looping defends against spurious wake-ups.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[panic_handler]
fn panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
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
