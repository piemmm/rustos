//! `extern "C" fn rustos_arch_aarch64_main` — the Rust side of the boot
//! trampoline.
//!
//! The assembly in `boot.s` finishes by `bl`-ing this symbol with the
//! firmware hand-off preserved (`x0 = DTB pointer`; AAPCS64 calling
//! convention). This function transfers control to the binary-supplied
//! `extern "C" fn kernel_main(dtb) -> !`, mirroring the riscv64 port's
//! `rustos_arch_riscv64_main` seam: each test binary (and the production
//! kernel binary) defines `kernel_main` exactly once.

extern "C" {
    /// Provided by the linked binary. Must not return.
    ///
    /// `dtb` is the physical address of the flattened device tree, as
    /// handed over by QEMU's boot loader and forwarded verbatim by
    /// `boot.s`.
    fn kernel_main(dtb: u64) -> !;
}

/// The trampoline jumps here, exactly once, on the boot CPU.
///
/// The DTB is validated by the boot pipeline if and when it parses it;
/// this seam exists so the assembly hands off to Rust as early as
/// possible.
///
/// # Safety
///
/// Implicitly safe to call from the asm trampoline because `boot.s`'s
/// SAFETY-INVARIANTs hold (EL1, interrupts masked, valid `x0`, stack
/// established). Calling from anywhere else is a kernel bug.
#[no_mangle]
pub extern "C" fn rustos_arch_aarch64_main(dtb: u64) -> ! {
    // SAFETY: `kernel_main` is provided by the linked binary and is
    // documented `-> !` (see the `extern` block). Forwarding the
    // verbatim hand-off value once is the entire contract.
    unsafe { kernel_main(dtb) }
}
