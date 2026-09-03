//! x86_64 terminal platform states: warm reboot, and parking a CPU.
//!
//! x86 has no single architected "reset the machine" instruction, so a warm
//! reboot is driven through the legacy platform hardware every PC-class
//! chipset (and the QEMU `pc`/`q35` machines) implements:
//!
//! * the Intel 8042 keyboard controller's command port `0x64`, whose
//!   `0xFE` ("pulse output line") command asserts the CPU `INIT#`/reset
//!   line — the oldest and most universally supported PC reset, and
//! * the PCI reset-control register at I/O port `0xCF9`, whose
//!   `SYS_RST | RST_CPU` (`0x06`) then full-reset (`0x0E`) sequence the
//!   ICH/PCH (and QEMU's `q35`) decode as a hard reset — the fallback
//!   when no 8042 is present.
//!
//! Both are attempted in turn; on success the platform resets and control
//! never comes back, so [`reboot`] only returns when *neither* channel
//! exists (a platform with no 8042 and no `0xCF9`), leaving the caller to
//! report the reset as unsupported and fail safe.
//!
//! Power-off (ACPI S5) is deliberately **not** implemented here: it
//! requires parsing the FADT and the DSDT `\_S5` object to learn the
//! `PM1a` control register and its `SLP_TYP` value, which belongs to the
//! ACPI power-management subsystem, not this small reset shim. Until that
//! subsystem exists the x86_64 port reports power-off as unsupported (the
//! `KernelArch::poweroff` default), which is honest rather than writing a
//! guessed, chipset-specific control port from here.

/// Park the calling CPU forever: mask its interrupts, then halt.
///
/// The port's one parked-CPU sequence — the fail-closed terminus of a
/// fatal report, an unexpected interrupt, a stop-request acknowledge, and
/// the `hlt` behind QEMU's exit byte. Only `NMI` and `SMI` can wake a
/// `cli`'d `hlt`; both return here and re-halt, which is what makes the
/// `!` honest.
///
/// Lives here rather than in `kernel_arch` because that module is gated on
/// the `sched-arch` feature, and a freestanding consumer without the Arch
/// HAL still has to be able to park.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn park_cpu() -> ! {
    // SAFETY: `cli` and `hlt` are serialising instructions documented in
    // Intel SDM Vol 2B (CLI) and Vol 2A (HLT). They touch no memory and
    // have no calling-convention side effects.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
    }
    loop {
        // SAFETY: as above. `IF` is masked, so only `NMI`/`SMI` wake the
        // CPU; both land back here and re-execute `hlt`.
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Host twin of the freestanding park: a host build has no `hlt`, and no
/// host test calls this (the `-> !` signature is proven by a compile-time
/// assertion, never by blocking a test thread).
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
pub fn park_cpu() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// Reset control register I/O port (the ICH/PCH "reset control" register,
/// PIIX/ICH datasheets). Byte-wide.
const RESET_CONTROL_PORT: u16 = 0xCF9;

/// `0xCF9` value asserting `SYS_RST` (bit 1) — arm the reset.
const RESET_CONTROL_ARM: u8 = 0x02;

/// `0xCF9` value asserting `SYS_RST | RST_CPU` (bits 1 and 3) — trigger a
/// full hardware reset.
const RESET_CONTROL_FULL: u8 = 0x0E;

/// Intel 8042 keyboard-controller command port. Byte-wide.
const KBC_COMMAND_PORT: u16 = 0x64;

/// 8042 "pulse output line" command asserting the CPU reset line.
const KBC_PULSE_RESET: u8 = 0xFE;

// The reset-control and 8042 command values are fixed by the PC platform,
// not free to drift. Asserted at compile time (on every target, so the
// constants are never dead code) rather than in a host test, since the
// `out` instructions themselves cannot run from a host unit test.
const _: () = {
    assert!(RESET_CONTROL_PORT == 0xCF9);
    assert!(RESET_CONTROL_ARM == 0x02); // SYS_RST
    assert!(RESET_CONTROL_FULL == 0x0E); // SYS_RST | RST_CPU
    assert!(KBC_COMMAND_PORT == 0x64);
    assert!(KBC_PULSE_RESET == 0xFE); // pulse output line / reset
};

/// Warm-reboot the machine through the legacy PC reset hardware.
///
/// Pulses the 8042 controller's reset line, then, if that did not reset the
/// machine, drives the `0xCF9` reset-control register. On success the
/// platform resets and this never returns; it returns only when neither
/// channel is present, so the caller reports the reboot as unsupported and
/// carries on (fail safe).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn reboot() {
    // SAFETY: each `out` is a side-effect-only byte write to a fixed legacy
    // reset-control port (`0x64` 8042 command, `0xCF9` PCI reset control).
    // The kernel runs in ring 0 with port-I/O permission throughout, the
    // writes touch no memory, and on a platform that decodes them the
    // machine resets before the next instruction retires.
    unsafe {
        // Try the 8042 pulse-reset first (present on every PC-class board
        // and the QEMU `pc`/`q35` machines).
        out8(KBC_COMMAND_PORT, KBC_PULSE_RESET);
        // If the machine is still running, fall back to the `0xCF9`
        // reset-control register: arm `SYS_RST`, then request the full
        // CPU reset.
        out8(RESET_CONTROL_PORT, RESET_CONTROL_ARM);
        out8(RESET_CONTROL_PORT, RESET_CONTROL_FULL);
    }
    // Both channels were no-ops (a platform with neither): return so the
    // caller reports the reboot as unsupported rather than assuming success.
}

/// Off the freestanding x86_64 target there is no legacy port-I/O space, so
/// reset is unsupported: return immediately (fail safe). Present so the bin
/// crate's `KernelArch::reboot` calls one shape on every build.
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
pub fn reboot() {}

/// Write one byte to a legacy I/O `port`.
///
/// # Safety
///
/// The caller must run with ring-0 port-I/O permission and `port` must be a
/// legacy control port for which a byte write is well-defined. The write
/// has no memory side effects.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[inline]
unsafe fn out8(port: u16, value: u8) {
    // SAFETY: `out dx, al` is the documented byte PIO write; the caller's
    // contract guarantees ring-0 permission and a valid control port.
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reboot_is_a_no_op_off_the_freestanding_target() {
        // The host build has no legacy port space; `reboot` must return
        // (unsupported) rather than emit an invalid instruction.
        reboot();
    }
}
