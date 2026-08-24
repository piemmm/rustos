//! QEMU `SiFive` Test (`sifive_test`) finisher helper.
//!
//! The `qemu-system-riscv64 -M virt` board exposes a `SiFive` Test
//! device at MMIO base [`SIFIVE_TEST_BASE`]. Writing a 32-bit finisher
//! word there terminates the guest:
//!
//! * [`FINISHER_PASS`] makes QEMU exit the host process with status `0`.
//! * A failure word — [`FINISHER_FAIL`] in the low half with a caller
//!   code in the high half (see [`fail_word`]) — makes QEMU exit with
//!   that code.
//!
//! Zero is therefore this board's *success* status, which is why a
//! failure code is a [`NonZeroU16`]: a zero-coded failure would exit
//! QEMU with status `0` and the runner would read the failing run as a
//! pass.
//!
//! The host-side [`tairix_qemu`][crate-runner] crate decodes that status
//! back into Pass/Fail (zero ⇒ Pass on this board, unlike x86_64's
//! `isa-debug-exit` where success is a *non-zero* status). The constants
//! here therefore must stay in sync with `tools/qemu/src/riscv64.rs` —
//! they are duplicated as a small set of `const`s on each side rather
//! than shared through a common crate so the kernel side of the contract
//! has zero dependencies beyond `core`. The `constants_match_runner`
//! unit test is that tie-down.
//!
//! [crate-runner]: ../../../tools/qemu/src/riscv64.rs

use core::num::NonZeroU16;

/// MMIO base address of the `virt` board's `SiFive` Test device. Must
/// match `tairix_qemu::riscv64::SIFIVE_TEST_BASE`.
pub const SIFIVE_TEST_BASE: u64 = 0x10_0000;

/// Finisher word reported on a successful test. QEMU exits the host
/// process with status `0`. Must match
/// `tairix_qemu::riscv64::FINISHER_PASS`.
pub const FINISHER_PASS: u32 = 0x5555;

/// Low half of every failure finisher word. The high 16 bits carry the
/// caller's exit code (see [`fail_word`]). Must match
/// `tairix_qemu::riscv64::FINISHER_FAIL`.
pub const FINISHER_FAIL: u32 = 0x3333;

/// Build the failure finisher word for `code`.
///
/// The `SiFive` Test device interprets a write whose low 16 bits equal
/// [`FINISHER_FAIL`] as "exit the host process with the status carried in
/// the high 16 bits". A [`NonZeroU16`] code therefore cannot encode the
/// zero status the runner reads as a pass. Keeping the shift/mask in a
/// pure function lets the host tests pin both properties without a
/// riscv64 target.
#[must_use]
pub const fn fail_word(code: NonZeroU16) -> u32 {
    ((code.get() as u32) << 16) | FINISHER_FAIL
}

/// Tell QEMU the test passed and **never return**.
///
/// Writes [`FINISHER_PASS`] to the `SiFive` Test device, then parks the
/// hart in a `wfi` loop. The park is unreachable under QEMU (the write
/// terminates the process) but is the correct conservative behaviour on
/// hardware without the device.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub fn exit_success() -> ! {
    // SAFETY: `FINISHER_PASS` to `SIFIVE_TEST_BASE` is the documented
    // QEMU `virt` contract; the `wfi` park afterwards is unreachable on
    // QEMU but is a defence-in-depth termination path on bare metal.
    unsafe { write_finisher(FINISHER_PASS) };
    park_forever();
}

/// Tell QEMU the test failed with `code` and **never return**.
///
/// See [`exit_success`]; this differs only in writing [`fail_word`]`(code)`.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub fn exit_failure(code: NonZeroU16) -> ! {
    // SAFETY: identical to `exit_success`; see that function.
    unsafe { write_finisher(fail_word(code)) };
    park_forever();
}

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
#[inline]
unsafe fn write_finisher(word: u32) {
    // SAFETY: the caller guarantees a riscv64 `virt`-board environment
    // running in S-mode where the `SiFive` Test device is mapped at
    // `SIFIVE_TEST_BASE`. The write is a single naturally-aligned 32-bit
    // store to that fixed device register and touches no Rust-managed
    // memory.
    unsafe {
        core::ptr::write_volatile(SIFIVE_TEST_BASE as *mut u32, word);
    }
}

#[cfg(all(target_arch = "riscv64", target_os = "none"))]
fn park_forever() -> ! {
    loop {
        // SAFETY: `wfi` is a well-defined wait-for-interrupt hint on
        // riscv64. Looping defends against a wake-up.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_runner() {
        // The host runner's values live in `tools/qemu/src/riscv64.rs`;
        // this is the tie-down for the duplicated pair.
        assert_eq!(SIFIVE_TEST_BASE, 0x10_0000);
        assert_eq!(FINISHER_PASS, 0x5555);
        assert_eq!(FINISHER_FAIL, 0x3333);
    }

    #[test]
    fn fail_word_packs_code_into_high_half() {
        // `(code << 16) | FINISHER_FAIL` — the encoding QEMU's
        // `sifive_test` device decodes into the host-process exit code.
        assert_eq!(fail_word(NonZeroU16::MIN), (1 << 16) | 0x3333);
        assert_eq!(fail_word(NonZeroU16::new(42).unwrap()), (42 << 16) | 0x3333);
        assert_eq!(fail_word(NonZeroU16::MAX), 0xFFFF_3333);
    }

    #[test]
    fn no_reportable_code_exits_with_the_pass_status() {
        // The high half is the status QEMU reports, and the runner reads `0`
        // as a pass. Widening `code` to `u16` admits `fail_word(0)`, whose
        // high half is zero — a failing run reported as a passing one.
        for raw in 1..=u16::MAX {
            let word = fail_word(NonZeroU16::new(raw).unwrap());
            assert_eq!(word & 0xFFFF, FINISHER_FAIL);
            assert_eq!(word >> 16, u32::from(raw));
            assert_ne!(word >> 16, 0, "code {raw} would exit QEMU with status 0");
        }
    }
}
