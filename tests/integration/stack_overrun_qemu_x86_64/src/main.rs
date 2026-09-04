//! `plans/OPEN-DEFECTS.md` D82 QEMU integration test (`x86_64`): a live,
//! scheduled kthread running on a **window-backed** kernel stack takes a
//! synchronous, supervisor-mode not-present page fault the instant it
//! overruns into the stack's guard slot. The x86_64 sibling of
//! `tests/integration/stack_overrun_qemu_aarch64`.
//!
//! ## Why this exists
//!
//! A kthread kernel stack is a run of pages in the shared kernel remap
//! window whose lowest slot is reserved but never mapped. Because that
//! window's sub-hierarchy is installed by every translation root, the guard
//! is absent everywhere at once — nothing refines a live huge page, and no
//! root carries a per-task unmap. This vertical proves the payoff: an
//! *overrunning kthread* faults synchronously in hardware rather than being
//! caught at its next reschedule by the software-canary
//! `tairix_kernel_core::kthread::BoxStack` fallback.
//!
//! ## What this test asserts
//!
//! x86_64 long-mode bring-up (GDT, TSS, the dedicated error-code-aware
//! `#PF` entry, the bump heap) is the production boot pipeline's job, so
//! this test boots the real `tairix-kernel` pipeline — which also installs
//! the remap window and the stack tier — and, on
//! `AuditEvent::BootCompleted`:
//!
//! 1. Draws one kthread kernel stack from the installed tier through the
//!    production `tairix_kernel_core::kstack::alloc_kernel_stack`. The
//!    active CR3 already carries the window's shared sub-hierarchy, so the
//!    run resolves without the test touching a page table.
//! 2. Checks the stack really came from the window rather than the
//!    software-canary heap fallback — a fallback here would mean the
//!    production install silently degraded — and that its usable run is
//!    mapped and writable.
//! 3. Builds the live `tairix_kernel_sched_eevdf::Scheduler` over
//!    `X86_64Arch` and admits a kthread on that stack via
//!    `spawn_kthread_with_stack` — the production runtime path, not a bare
//!    function call.
//! 4. The kthread body overruns: it writes the highest byte of its guard
//!    slot, the first byte a contiguous downward overrun crosses. Because
//!    that slot is unmapped, the access raises a synchronous,
//!    supervisor-mode not-present `#PF` while the kthread is *running*.
//! 5. The `tairix_arch_x86_64::fault` observer confirms the trap is a
//!    supervisor not-present fault on exactly the guard slot and reports
//!    PASS. A regression that left the slot mapped lets the body return
//!    cleanly; the cooperative `step` loop then drains the task and the test
//!    reports FAILURE explicitly rather than passing.

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
