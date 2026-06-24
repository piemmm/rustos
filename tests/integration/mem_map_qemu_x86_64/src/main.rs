//! SPAWN stage `SP5b-2` QEMU integration test (x86_64 sibling): prove a
//! ring-3 process can obtain and release anonymous `RW` memory at runtime via
//! `abi-v1` on x86_64 (`plans/SPAWN.md` SP5).
//!
//! Unlike the riscv64/aarch64 `mem_map` siblings — which stand up a minimal
//! self-contained test kernel — the x86_64 ring-3 transition needs the GDT
//! ring-3 selectors, the TSS, and `syscall`/`IA32_LSTAR` entry installed, so
//! this test boots the production `rustos-kernel` pipeline (exactly like the
//! sibling `rustos-test-spawn-program-qemu-x86_64`). That pipeline now also
//! installs the dedicated, error-code-aware page-fault entry
//! (`rustos_arch_x86_64::fault`), so the deliberate use-after-unmap `#PF` is
//! observable rather than fail-closed-and-opaque.
//!
//! On `AuditEvent::BootCompleted` the test enables `IA32_EFER.NXE`, installs a
//! `rustos_arch_x86_64::fault` observer, builds one hardware-isolated user
//! address space from the `rxe` fixture program through the production
//! capability-checked, audited `rustos_kernel_core::spawn_image` caller,
//! **retains it live**, and installs a `rustos_kernel_core::MemMap` producer
//! backed by `rustos_kernel_mem::map_anonymous` / `unmap_anonymous` over that
//! space and its frame pool. It then `iretq`s into the program directly
//! through `EnterUser::enter_user`; the dispatch callback routes the program's
//! `mem_map` / `mem_unmap` `syscall`s through the producer.
//!
//! The program maps an anonymous region (FIXED), writes and reads back a
//! pattern, unmaps it, then touches the released range — which must raise a
//! page fault the fault observer reports as PASS. Any shortfall (the program
//! returning a failure exit code, an unexpected syscall, or no fault) flips
//! `qemu_exit::exit_failure` or times out, so the run fails loudly.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-mem-map-qemu-x86_64: the `test-hooks` Cargo feature is a \
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
