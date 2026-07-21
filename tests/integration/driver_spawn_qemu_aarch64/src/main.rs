//! `PLAN.md` Stage 4.HW QEMU integration test: prove the kernel-side
//! production driver spawner end to end on the aarch64 `virt` board.
//!
//! The build script compiles the pure-Rust driver-stub program
//! (`tests/integration/driver_register_program`) position-independent and
//! converts it to an `rxe` blob carrying the kernel's compiled-in syscall CFI
//! tag, registered in the test kernel's `ProgramRegistry` under a
//! `/System/Drivers/` path. On boot the test discovers the board from the
//! embedded `virt` device tree, enables the identity MMU + EL1 vectors,
//! builds a live `FrameAllocator`, publishes the reply `Port` (send-gated on
//! a driver-class capability) into a live `RwLock<PortRegistry>` — both the
//! endpoint binding and its well-known port name — installs
//! the production `KernelDispatchHook` through a `DispatchCallbackSlot`, and
//! spawns the stub through the production parameterised
//! `Aarch64ProcessSpawn` image builder via the exported `KernelSpawnCtx` admit
//! path — driver-class caps plus the reply endpoint id in `arg(1)`, exactly
//! the hand-off the driver host gives a spawned driver process.
//!
//! The host side then drives the cooperative `step` loop, polling
//! `Port::recv` between steps under a bounded budget — the budget-bounded
//! cooperative wait of the Stage 4.HW handshake. The spawned stub reads
//! `arg(1)`, resolves the well-known reply port name from `arg(2)` over the
//! production `port_resolve` syscall (refusing to proceed unless it names
//! the same endpoint), sends `DriverRegisterReply::registered(...)` over the
//! production `ipc_send` syscall (caller-context resolution, copy-in from
//! its own isolated address space, capability-gated `Port::send`), and
//! exits. PASS once the decoded reply round-trips the stub's pinned handle;
//! any shortfall (spawn failure, missing or malformed reply, drained
//! workload without a reply) writes a distinct failure finisher or times out
//! (fail-loud).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-driver-spawn-qemu-aarch64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

#[cfg(all(itest_aarch64, feature = "test-hooks"))]
extern crate alloc;

#[cfg(all(itest_aarch64, feature = "test-hooks"))]
mod kernel;

// --- Stub when the test-hooks feature is off ----------------------
#[cfg(all(itest_aarch64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_dtb: u64) -> ! {
    loop {
        // SAFETY: `wfe` is a well-defined parked-CPU hint on aarch64.
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(all(itest_aarch64, not(feature = "test-hooks")))]
#[panic_handler]
fn panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
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
