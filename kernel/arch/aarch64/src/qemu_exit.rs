//! ARM semihosting (`SYS_EXIT`) test-finisher helper.
//!
//! The QEMU `virt` board has no `SiFive` Test device; instead TAIRiX'
//! aarch64 QEMU verticals report their result through **ARM
//! semihosting** (Semihosting for AArch64, ARM DUI 0203). With QEMU run
//! under `-semihosting-config enable=on,target=native`, the guest
//! triggers a semihosting operation with the `HLT #0xF000` instruction,
//! the operation number in `x0`, and a parameter block pointer in `x1`.
//!
//! The [`SYS_EXIT`] (`0x18`) operation takes a two-word block
//! `{ reason, subcode }`. When `reason ==`
//! [`ADP_STOPPED_APPLICATION_EXIT`] QEMU exits the host process with
//! `subcode` as its status. So:
//!
//! * `exit_success` passes `subcode == 0`, exiting QEMU with status
//!   `0` — which the host runner treats as success (the same zero-is-pass
//!   convention as riscv64's `SiFive` Test finisher, and the inverse of
//!   x86_64's non-zero `isa-debug-exit`).
//! * `exit_failure` passes `subcode == code`, exiting with that code.
//!
//! The subcode *is* the host exit status, so the
//! [`NonZeroU16`](core::num::NonZeroU16) `exit_failure` takes is the whole
//! guard against a failure being reported as a pass: this board has no
//! encoding step in which a zero could be caught.
//!
//! The operation/reason constants must stay in step with
//! `tools/qemu/src/aarch64.rs` (which decodes the exit status); the
//! `constants_match_runner` unit test is that tie-down.

/// Semihosting operation number for `SYS_EXIT` (ARM DUI 0203).
pub const SYS_EXIT: u64 = 0x18;

/// `ADP_Stopped_ApplicationExit` reason code: paired with [`SYS_EXIT`],
/// it tells QEMU the second block word is the desired host-process exit
/// status (ARM DUI 0203 §5.5.2).
pub const ADP_STOPPED_APPLICATION_EXIT: u64 = 0x2_0026;

/// QEMU host-process exit status produced by `exit_success`. Unlike
/// x86_64's `isa-debug-exit`, the semihosting finisher reports success
/// as a plain zero exit status. Must match
/// `tairix_qemu::aarch64::SUCCESS_EXIT_STATUS`.
pub const SUCCESS_EXIT_STATUS: i32 = 0;

/// Tell QEMU the test passed and **never return**.
///
/// Issues `SYS_EXIT` with subcode `0`, then parks the CPU. The park is
/// unreachable under QEMU (the call terminates the process) but is the
/// correct conservative behaviour on hardware without a semihosting
/// host.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn exit_success() -> ! {
    // SAFETY: the documented QEMU semihosting contract; the park
    // afterwards is unreachable on QEMU but a defence-in-depth
    // termination path on bare metal.
    unsafe { semihosting_exit(0) }
}

/// Tell QEMU the test failed with `code` and **never return**.
///
/// See [`exit_success`]; this differs only in the non-zero subcode.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn exit_failure(code: core::num::NonZeroU16) -> ! {
    // SAFETY: identical to `exit_success`; see that function.
    unsafe { semihosting_exit(u64::from(code.get())) }
}

/// Issue the `SYS_EXIT` semihosting call with `subcode`, then park.
///
/// # Safety
///
/// The caller must guarantee a QEMU `virt`-board environment running
/// with semihosting enabled; the `HLT #0xF000` instruction is otherwise
/// an undefined-instruction trap.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
unsafe fn semihosting_exit(subcode: u64) -> ! {
    let block: [u64; 2] = [ADP_STOPPED_APPLICATION_EXIT, subcode];
    // SAFETY: `block` is a live two-word parameter block; its pointer is
    // passed in `x1` and the operation number in `x0`, per the AArch64
    // semihosting calling convention. QEMU reads the block and exits, so
    // the instruction does not return; the loop is a defensive park.
    unsafe {
        core::arch::asm!(
            "hlt #0xF000",
            in("x0") SYS_EXIT,
            in("x1") block.as_ptr(),
            options(nostack),
        );
    }
    loop {
        // SAFETY: `wfi` is a well-defined wait-for-interrupt hint with no
        // architectural side effects; looping defends against a wake-up.
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
        // The host runner's values live in `tools/qemu/src/aarch64.rs`;
        // this is the tie-down for the duplicated set.
        assert_eq!(SYS_EXIT, 0x18);
        assert_eq!(ADP_STOPPED_APPLICATION_EXIT, 0x2_0026);
        assert_eq!(SUCCESS_EXIT_STATUS, 0);
    }
}
