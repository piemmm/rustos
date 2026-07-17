//! `plans/PI.md` guard-page fault-form (x86_64 stage G1/G2) QEMU
//! integration test: the x86_64 four-level huge-page split turns a single
//! 4 KiB page inside a coarse identity 2 MiB huge page into an unmappable
//! page, so a supervisor-mode access to it raises a synchronous,
//! not-present page fault. The sibling of
//! `tests/integration/stack_guard_qemu_{aarch64,riscv64}`, and the proof
//! that x86_64 — the last `BlockSplit::Pending` port — is now
//! `BlockSplit::Supported`.
//!
//! ## Why this exists
//!
//! The kthread kernel-stack guard (`kernel/core::kthread`) catches a stack
//! overflow with a poison canary checked at the next reschedule (the
//! binding defence). The *deployment* form turns the overflow into
//! an immediate hardware fault by **unmapping** the guard page. But the
//! boot path identity-maps RAM with coarse 2 MiB (and, where available,
//! 1 GiB) *huge pages*, and such a leaf has no per-4 KiB entry to clear —
//! so the region must first be re-expressed at 4 KiB granularity. That is
//! exactly `AddressSpace::split_block`, and this vertical proves the live
//! mechanism end to end on the production x86_64 pipeline.
//!
//! ## What this test asserts
//!
//! On `AuditEvent::BootCompleted` (the production boot pipeline has
//! installed the GDT, the dedicated error-code-aware `#PF` entry, and the
//! bump heap):
//!
//! 1. Build a `paging::AddressSpace` identity-mapping the low 4 GiB with
//!    2 MiB huge pages (so the running RIP / stack / per-CPU TLS and the
//!    guard static's low-identity physical alias all stay reachable across
//!    the CR3 switch).
//! 2. Activate it (load CR3).
//! 3. `split_block(guard_phys)`: shatter the 2 MiB huge page covering the
//!    guard static into 512 × 4 KiB pages, preserving every mapping. The
//!    split only *adds* table levels reproducing the existing translation,
//!    so it is safe against the running regime.
//! 4. Write a sentinel through the guard page's low-identity alias and read
//!    it back: the split preserved the mapping under the live MMU (a
//!    regression here reports FAILURE, it does not hang).
//! 5. `unmap(guard_phys)` + `flush_page(guard_phys)`: tear the single page
//!    down through the Arch HAL and flush its stale TLB entry. The kernel's
//!    code / stack live at higher-half virtual addresses (a different
//!    region) and stay mapped.
//! 6. Read the guard page's low-identity alias: the MMU raises a
//!    not-present page fault; the `tairix_arch_x86_64::fault` observer
//!    confirms it is a supervisor not-present fault on exactly that page
//!    and reports PASS. A regression that left the page mapped reads it
//!    without faulting and reports FAILURE explicitly.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-stack-guard-qemu-x86_64: the `test-hooks` Cargo feature is a \
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
