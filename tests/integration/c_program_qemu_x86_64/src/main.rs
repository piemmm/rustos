//! CCOMPAT stage CC5 QEMU integration test: spawn a genuinely C-compiled
//! program (clang object + crt0 + abi-sys staticlib) into **ring 3** on x86_64
//! and assert it exits `99` — i.e. every abi-v1 C-header check and both syscall
//! round-trips it performs succeed.
//!
//! Unlike the riscv64/aarch64 C-program round-trips — which stand up a minimal
//! self-contained test kernel — the x86_64 ring-3 transition needs the GDT
//! ring-3 selectors, the TSS, and `syscall`/`IA32_LSTAR` entry installed, so
//! this test boots the production `tairix-kernel` pipeline (exactly like the
//! sibling `tairix-test-spawn-program-qemu-x86_64`). On
//! `AuditEvent::BootCompleted` it enables `IA32_EFER.NXE`, builds a fresh
//! address space (low 32 MiB identity + the higher-half kernel window),
//! switches CR3, installs a dispatch callback, then calls the production
//! capability-checked, audited spawn caller
//! (`tairix_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to
//! materialise the program's ring-3 image — built from the `rxe` blob the build
//! script produced, with W^X leaf permissions (code RX, data RW-NX, rodata
//! R-NX) — and `iretq` into it. The C program checks a Time64 value across the
//! boundaries, an ipc header, and a sysinfo header, then calls `cap_query`
//! and `clock_get`; the dispatch callback services those two syscalls
//! (returning a known answer / sentinel) and finally asserts the `exit` code is
//! `99` before `qemu_exit::exit_success`.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-c-program-qemu-x86_64: the `test-hooks` Cargo feature is a \
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
