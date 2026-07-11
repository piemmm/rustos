//! `SP11e` QEMU integration test: prove the demand-grown user stack end to
//! end on the QEMU x86_64 machine (`docs/src/architecture/memory.md` §7c) —
//! the twin of the aarch64/riscv64 stack-grow verticals.
//!
//! On boot the test runs the shared production board bring-up
//! (`rustos_kernel::x86_64::boot::bring_up_bsp`) — per-CPU tables, the
//! dedicated `#PF` entry, NXE, LAPIC calibration, the ACPI walk, the
//! **production** syscall-dispatch callback and user-fault resolver, the
//! `syscall`/TSS entry, and the masked IO-APIC routing — then installs the
//! production `KernelDispatchHook` into the production `DISPATCH_SLOT`
//! those callbacks resolve through, with the production `LiveMemMap`
//! producer backing both `mem_map` and the stack-growth fault path, a real
//! `KernelProcessWait`, and a `ProgramRegistry` carrying the three child
//! roles (their numeric parameters derived from the one shared
//! `spawn_layout` policy). The parent role of the fixture program
//! (`tests/integration/stack_grow_program`) is spawned through the
//! production `InitSpawnCtx::spawn_driver_process` seam; it drives the
//! children through the production `spawn` + `wait` syscalls:
//!
//! * `grow` recurses far past the eagerly committed stack top — the run
//!   only completes if the fault-driven growth path backs every page — and
//!   verifies each frame's bytes survived, exiting 0;
//! * `limit` lowers its own `StackBytes` soft bound through `rlimit_set`
//!   and recurses past it — the parent observes exit 139 through `wait`
//!   (the audited `stack_limit` fault-kill);
//! * `guard` reads the unmapped guard page below the reserved span — the
//!   parent observes exit 139 (the guard stays the structural defence).
//!
//! The chassis reaps the parent through the wait producer's non-blocking
//! poll; PASS fires only on a parent exit of 0. Any child or parent
//! misbehaviour surfaces as a distinct logged failure site (the parent's
//! diagnostic exit code is logged as a field — the x86_64 `isa-debug-exit`
//! device carries no per-site code), and a wedged run times out through
//! the harness (fail-loud, `AGENTS.md` §7).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-stack-grow-qemu-x86_64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

#[cfg(all(itest_x86_64, feature = "test-hooks"))]
extern crate alloc;

#[cfg(all(itest_x86_64, feature = "test-hooks"))]
mod kernel;

// --- Stub when the test-hooks feature is off ----------------------
#[cfg(all(itest_x86_64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_boot_info: u64) -> ! {
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
