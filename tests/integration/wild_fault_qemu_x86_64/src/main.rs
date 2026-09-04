//! QEMU integration test for `plans/OPEN-DEFECTS.md` D42 and D86: on
//! x86_64, a ring-3 exception the kernel cannot resolve kills the faulting
//! task and leaves the CPU running.
//!
//! Before the user-fault terminator was wired, two shapes of unprivileged
//! exception ended the *machine*: a ring-3 instruction-fetch `#PF` (a wild
//! jump) escaped the resolver's data-only gate and fell through to the fatal
//! report (D42), and every ring-3 exception other than `#PF` reached that
//! same report because the port had no terminator to route to (D86). Either
//! was a denial of service any process could trigger.
//!
//! On boot the test runs the shared production board bring-up
//! (`tairix_kernel::x86_64::boot::bring_up_bsp`) — per-CPU tables, the
//! per-vector exception stubs, the dedicated `#PF` entry, NXE, LAPIC
//! calibration, the ACPI walk, the **production** syscall-dispatch callback,
//! user-fault resolver and user-fault terminator, the `syscall`/TSS entry,
//! and the masked IO-APIC routing — then installs the production
//! `KernelDispatchHook` into the production `DISPATCH_SLOT` those callbacks
//! resolve through, with a real `KernelProcessWait` and a `ProgramRegistry`
//! carrying the three faulting child roles. The parent role of the fixture
//! program (`tests/integration/wild_fault_program`) is spawned through the
//! production `InitSpawnCtx::spawn_driver_process` seam; it drives the
//! children through the production `spawn` + `wait` syscalls:
//!
//! * `jump` calls a function pointer into its own No-Execute data image —
//!   the ring-3 instruction-fetch `#PF` the resolver never sees (D42);
//! * `ud` executes the always-invalid opcode — a ring-3 `#UD`, vector 6,
//!   for which the CPU pushes no hardware error code (D86);
//! * `gp` executes a privileged instruction — a ring-3 `#GP`, vector 13,
//!   for which it does, so both exception-stub shapes are covered.
//!
//! Each must be reaped with exit 139 (`128 + SIGSEGV`). The parent's own
//! survival is the load-bearing assertion: it can only reap a third child if
//! neither of the first two parked the CPU, which is precisely what both
//! defects did.
//!
//! The chassis reaps the parent through the wait producer's non-blocking
//! poll; PASS fires only on a parent exit of 0. Any child or parent
//! misbehaviour surfaces as a distinct logged failure site (the parent's
//! diagnostic exit code is logged as a field — the x86_64 `isa-debug-exit`
//! device carries no per-site code), and a wedged run times out through the
//! harness (fail-loud).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-wild-fault-qemu-x86_64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds."
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
