//! `extern "C" fn rustos_arch_x86_64_main` — the Rust side of the boot
//! trampoline.
//!
//! The 32-bit assembly in `boot.s` finishes by `call`ing this symbol with
//! the multiboot1 magic in `%rdi` and the multiboot info pointer in
//! `%rsi` (System V AMD64 ABI, see `boot.s` SAFETY-INVARIANT 7).
//!
//! This function performs the small set of platform-bring-up steps the
//! Stage-2 tests need and then transfers control to a binary-supplied
//! `extern "C" fn kernel_main() -> !`. Every test binary defines that
//! symbol exactly once.

use crate::{qemu_exit, MULTIBOOT2_BOOTLOADER_MAGIC};

extern "C" {
    /// Provided by the test binary. Must not return.
    fn kernel_main() -> !;
}

/// The trampoline jumps here. Called *exactly once* on the boot CPU.
///
/// # Behaviour
///
/// 1. Validates the multiboot magic; mismatched magic is a closed-fail
///    (`AGENTS.md` §5.4.3 — validate every input).
/// 2. Transfers to the binary-supplied `kernel_main`.
///
/// IDT installation is deferred to `kernel_main` because each test
/// installs its *own* page-fault handler. This avoids the alternative
/// (a kernel-side IDT that has to be re-pointed) which would touch a
/// `static mut` from two call sites — forbidden by `AGENTS.md` §2.
///
/// # Safety
///
/// Implicitly safe to call from the asm trampoline because the
/// invariants in `boot.s` are upheld. Calling from anywhere else is a
/// kernel bug.
#[no_mangle]
pub extern "C" fn rustos_arch_x86_64_main(magic: u64, _multiboot_info: u64) -> ! {
    if (magic as u32) != MULTIBOOT2_BOOTLOADER_MAGIC {
        // Mismatched multiboot magic means we were entered by something
        // other than a multiboot2 loader; fail closed.
        qemu_exit::exit_failure();
    }
    // SAFETY: `kernel_main` is provided by the linked test binary and is
    // documented as `-> !` (see `extern` block above). Calling it once is
    // the entire contract.
    unsafe { kernel_main() }
}
