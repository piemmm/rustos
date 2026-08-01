//! FIX-IO `IO2` QEMU integration test: prove on the aarch64 `virt` board
//! that **a wedged storage device fails closed at its own per-request
//! deadline and never stalls an unrelated healthy one** (`plans/FIX-IO.md`
//! IO2).
//!
//! On boot the test reads the GICv2 base + generic-timer rate from the
//! embedded `virt` device tree, brings up the EL1 vectors + GICv2, and
//! installs the **production** `KernelDispatchHook` through a
//! `DispatchCallbackSlot`, then spawns the fixture program
//! (`tests/integration/blkio_fault_program`) through the production
//! `InitSpawnCtx::spawn_driver_process` seam. Every syscall the fixture
//! issues therefore runs the shipped handler path, so the ticketed call
//! endpoint, the per-request deadline, the `CallReply` wait-set source, and
//! the reap/cancel paths under test are the real kernel objects. The drive
//! loop performs the timed-wake sweep a production timer tick would, which
//! is what lets an elapsed deadline wake the fixture's parked reaper.
//!
//! The fixture stands up two block-service endpoints of its own — one served
//! through the shared `blkio::serve_request_recovering` engine over a
//! fault-injecting device, one deliberately never serviced — and drives them
//! through the production `RemoteBlock` consumer to prove, through real
//! traps:
//!
//! * a transient device blip is **ridden out** inside the shared per-class
//!   reissue budget, returning correct data, with the device confirming it
//!   really injected the faults;
//! * a blip that outlasts that budget still fails closed deterministically,
//!   as the typed transient class;
//! * a bad sector keeps its own typed medium-error class rather than
//!   collapsing into a whole-device fault;
//! * a request outstanding to the wedged device neither stalls the healthy
//!   device nor completes early; its elapsed deadline wakes the parked
//!   reaper exactly like a real completion, and the claim then fails closed
//!   as a timeout;
//! * a per-ticket cancel withdraws an outstanding request, while a foreign
//!   ticket cancels nothing.
//!
//! The chassis reaps the fixture through the wait producer's non-blocking
//! poll; PASS fires only on a fixture exit of 0. Any misbehaviour surfaces
//! as a distinct failure finisher (the fixture's diagnostic exit code is
//! folded into the finisher), and a wedged run trips the chassis watchdog
//! (fail-loud).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-blkio-fault-qemu-aarch64: the `test-hooks` Cargo feature \
     is a debug-only test affordance and must not be enabled in release \
     builds (fail closed, never ship a test hook)."
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

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// kernel body is not compiled, so this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(itest_aarch64))]
fn main() {}
