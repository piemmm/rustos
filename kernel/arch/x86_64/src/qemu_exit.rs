//! QEMU `isa-debug-exit` helper.
//!
//! Writing a single byte to I/O port `0xf4` (configured via `-device
//! isa-debug-exit,iobase=0xf4,iosize=0x4`) causes QEMU to exit with status
//! `(byte << 1) | 1`. The host-side [`rustos_qemu`][crate-runner] crate
//! decodes that status back into Pass/Fail. The byte values here therefore
//! must stay in sync with `tools/qemu/src/lib.rs` — they're duplicated as a
//! pair of `const u8`s on each side rather than shared through a common
//! crate so that the kernel side of the contract has zero dependencies
//! beyond `core` and `core::arch`.
//!
//! [crate-runner]: ../../../tools/qemu/src/lib.rs

/// Byte value reported on a successful test. Must match
/// `rustos_qemu::SUCCESS_EXIT_CODE`.
pub const SUCCESS: u8 = 0x10;

/// Byte value reported on a failed test. Must match
/// `rustos_qemu::FAILURE_EXIT_CODE`.
pub const FAILURE: u8 = 0x11;

/// I/O port the `isa-debug-exit` device listens on.
pub const PORT: u16 = 0xf4;

/// Tell QEMU the test passed and **never return**.
///
/// # Safety
///
/// Issues a single `outb` to the `isa-debug-exit` I/O port. The instruction
/// itself is well-defined on every x86_64 CPU; the operation is `unsafe`
/// only because `outb` is an unconditional `unsafe` operation in Rust. If
/// QEMU is not running with the `isa-debug-exit` device attached the write
/// is a silent no-op and the subsequent `hlt` loop parks the CPU, which is
/// the correct conservative behaviour.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn exit_success() -> ! {
    // SAFETY: `outb` to `PORT` is the documented QEMU contract; the `hlt`
    // loop afterwards is unreachable on QEMU but is a defence-in-depth
    // termination path on bare metal per AGENTS.md §2.9.
    unsafe { write_port(SUCCESS) };
    halt_forever();
}

/// Tell QEMU the test failed and **never return**.
///
/// # Safety
///
/// See [`exit_success`]; this only differs in the byte written.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn exit_failure() -> ! {
    // SAFETY: identical to `exit_success`; see that function.
    unsafe { write_port(FAILURE) };
    halt_forever();
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[inline(always)]
unsafe fn write_port(value: u8) {
    // SAFETY: caller guarantees a valid x86_64 environment with port I/O
    // permission (ring 0, which is where the kernel runs throughout the
    // Stage-2 tests).
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") PORT,
            in("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn halt_forever() -> ! {
    loop {
        // SAFETY: `cli;hlt` is a well-defined parked-CPU sequence on x86_64
        // (`AGENTS.md` §2.9). Looping defends against spurious wake-ups.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_byte_matches_runner() {
        // Belt-and-braces cross-check: the host runner's value lives in
        // `tools/qemu/src/lib.rs`. AGENTS.md §2.2 forbids duplication
        // without a tie-down; this test is that tie-down.
        assert_eq!(SUCCESS, 0x10);
        assert_eq!(FAILURE, 0x11);
        assert_eq!(PORT, 0xf4);
    }
}
