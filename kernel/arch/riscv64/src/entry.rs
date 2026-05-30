//! `extern "C" fn rustos_arch_riscv64_main` — the Rust side of the boot
//! trampoline.
//!
//! The assembly in `boot.s` finishes by `call`ing this symbol with the
//! SBI hand-off registers preserved (`a0 = hartid`, `a1 = DTB pointer`;
//! System V riscv64 calling convention). This function transfers
//! control to the binary-supplied `extern "C" fn kernel_main(hartid,
//! dtb) -> !`, mirroring the x86_64 port's `rustos_arch_x86_64_main`
//! seam: each test binary (and the production kernel binary) defines
//! `kernel_main` exactly once.

extern "C" {
    /// Provided by the linked binary. Must not return.
    ///
    /// `hartid` is the boot hart id and `dtb` is the physical address
    /// of the flattened device tree, both as handed over by OpenSBI and
    /// forwarded verbatim by `boot.s`.
    fn kernel_main(hartid: u64, dtb: u64) -> !;
}

/// The trampoline jumps here, exactly once, on the boot hart.
///
/// There is no equivalent of x86_64's multiboot-magic validation: the
/// SBI boot protocol carries no magic, and the DTB is validated by
/// [`crate::fdt::Fdt`] when the boot pipeline parses it. This seam
/// exists so the assembly hands off to Rust as early as possible.
///
/// # Safety
///
/// Implicitly safe to call from the asm trampoline because `boot.s`'s
/// SAFETY-INVARIANTs hold (S-mode, paging off, valid `a0`/`a1`, stack
/// established). Calling from anywhere else is a kernel bug.
#[no_mangle]
pub extern "C" fn rustos_arch_riscv64_main(hartid: u64, dtb: u64) -> ! {
    // SAFETY: `kernel_main` is provided by the linked binary and is
    // documented `-> !` (see the `extern` block). Forwarding the
    // verbatim hand-off values once is the entire contract.
    unsafe { kernel_main(hartid, dtb) }
}
