//! Supervisor Binary Interface (SBI) calls.
//!
//! The boot pipeline needs a small, fixed set of SBI services — console
//! output, the timer, inter-processor interrupts, and secondary-hart
//! start — so this module exposes only those (no
//! bloat). Every kernel print (the boot log and the `BootCompleted`
//! audit record) goes out through the SBI legacy console, which OpenSBI
//! on the QEMU `virt` board routes to the same NS16550 UART that
//! `-serial stdio` captures.
//!
//! The legacy `console_putchar` call (extension id `0x01`) is used
//! rather than the newer Debug Console (`DBCN`) extension because every
//! OpenSBI build QEMU ships implements the legacy call, and the boot
//! pipeline's output is diagnostic only — the pass/fail result is
//! reported out-of-band through the `SiFive` Test device
//! (`qemu_exit`), so console availability is not load-bearing for the
//! test outcome.
//!
//! # IPI and hart start use the SBI v0.2+ extensions
//!
//! The timer and console use the v0.1 *legacy* extensions (each its own
//! extension id, a single `ecall` ABI every OpenSBI build implements).
//! Inter-processor interrupts (`send_ipi`) and secondary-hart bring-up
//! (`hart_start`) use the v0.2 **sPI** and **HSM** extensions instead:
//! the legacy `0x04` IPI call takes a *pointer* to a hart-mask word
//! (forcing the caller to materialise it in memory), and there is no
//! legacy hart-start at all. The v0.2 calls pass the mask by value and
//! return a typed [`SbiRet`], so the bring-up path observes failures
//! (fail closed) instead of a blind legacy call.

use tairix_arch_api::CpuId;

/// SBI legacy extension id for `set_timer`. Only the freestanding
/// build issues the call, so the constant is gated with it.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
const SBI_SET_TIMER: usize = 0x00;

/// SBI legacy extension id for `console_putchar`. Only the freestanding
/// build issues the call, so the constant is gated with it.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
const SBI_CONSOLE_PUTCHAR: usize = 0x01;

/// SBI v0.2 IPI extension id (ASCII `"sPI"`).
pub const SBI_EXT_IPI: usize = 0x73_5049;

/// `send_ipi` function id within the IPI extension.
pub const SBI_FID_SEND_IPI: usize = 0;

/// SBI v0.2 Hart State Management extension id (ASCII `"HSM"`).
pub const SBI_EXT_HSM: usize = 0x48_534D;

/// `hart_start` function id within the HSM extension.
pub const SBI_FID_HART_START: usize = 0;

/// SBI v0.2 Remote Fence (RFENCE) extension id (ASCII `"RFNC"`).
pub const SBI_EXT_RFENCE: usize = 0x5246_4E43;

/// `remote_sfence_vma` function id within the RFENCE extension.
pub const SBI_FID_REMOTE_SFENCE_VMA: usize = 1;

/// SBI v0.2 System Reset (SRST) extension id (ASCII `"SRST"`).
pub const SBI_EXT_SRST: usize = 0x5352_5354;

/// `system_reset` function id within the SRST extension.
pub const SBI_FID_SYSTEM_RESET: usize = 0;

/// SRST `reset_type` selecting an orderly shutdown / power-off.
pub const SBI_SRST_TYPE_SHUTDOWN: u32 = 0x0000_0000;

/// SRST `reset_type` selecting a cold reboot (a full platform reset).
pub const SBI_SRST_TYPE_COLD_REBOOT: u32 = 0x0000_0001;

/// SRST `reset_reason` denoting no specific reason (an operator-requested
/// reset, not a failure).
pub const SBI_SRST_REASON_NONE: u32 = 0x0000_0000;

/// `SbiRet` — the two-register return of every SBI v0.2 call.
///
/// `error` is `0` ([`SbiRet::SUCCESS`]) on success and a negative SBI
/// error code otherwise; `value` carries the call-specific payload (the
/// IPI and hart-start calls leave it `0`). Mirrors the C `struct
/// sbiret { long error; long value; }` the specification defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SbiRet {
    /// SBI error code (`0` on success; negative otherwise).
    pub error: isize,
    /// Call-specific return value.
    pub value: isize,
}

impl SbiRet {
    /// The `error` value denoting success (`SBI_SUCCESS`).
    pub const SUCCESS: isize = 0;

    /// `true` iff the call succeeded (`error == 0`).
    #[must_use]
    pub const fn is_success(self) -> bool {
        self.error == Self::SUCCESS
    }
}

/// Compute the `(hart_mask, hart_mask_base)` pair the SBI IPI / HSM
/// extensions take to address the single hart `hartid`.
///
/// The SBI v0.2 hart-mask convention selects hart `hart_mask_base + i`
/// for every set bit `i` of `hart_mask`. Basing the mask at `hartid`
/// itself means a single set bit (`1`) addresses exactly that hart,
/// regardless of how large the hart id is — there is no `1 << hartid`
/// shift to overflow for a high-numbered hart.
#[must_use]
pub const fn hart_mask_for(hartid: CpuId) -> (usize, usize) {
    (1, hartid as usize)
}

/// Program the next supervisor timer interrupt for absolute `time`
/// (in `time`-CSR ticks).
///
/// Issues the legacy `set_timer` `ecall` (extension id `0x00`). On
/// RV64 the 64-bit `stime_value` is passed whole in `a0`. The call
/// also clears any pending supervisor timer interrupt (`sip.STIP`), so
/// the timer trap handler re-arms the timer through this call to
/// acknowledge the tick. Errors are not observable through the legacy
/// ABI, so it never panics.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub fn set_timer(time: u64) {
    // SAFETY: an `ecall` with `a7 = 0x00` is the documented SBI legacy
    // `set_timer` service. It reads `a0` (the absolute deadline), writes
    // no guest memory, and clobbers only the SBI return registers
    // `a0`/`a1`, which are marked `lateout`.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") SBI_SET_TIMER,
            inout("a0") time => _,
            lateout("a1") _,
            options(nostack),
        );
    }
}

/// Write one byte to the SBI console.
///
/// Issues the legacy `console_putchar` `ecall`. Errors are not
/// observable through the legacy ABI (it returns no status), and the
/// console is diagnostic-only, so the call is treated as infallible —
/// it never panics.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub fn console_putchar(byte: u8) {
    // SAFETY: an `ecall` with `a7 = 0x01` is the documented SBI legacy
    // `console_putchar` service. It reads `a0` (the character), writes
    // no guest memory, and clobbers only the SBI-defined return
    // registers `a0`/`a1`, which are marked `lateout`.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") SBI_CONSOLE_PUTCHAR,
            inout("a0") usize::from(byte) => _,
            lateout("a1") _,
            options(nostack),
        );
    }
}

/// Send an inter-processor interrupt to every hart selected by
/// `(hart_mask, hart_mask_base)` via the SBI v0.2 IPI extension.
///
/// The targeted harts take a supervisor *software* interrupt
/// (`sip.SSIP` is set). Build the mask for a single hart with
/// [`hart_mask_for`]. Returns the [`SbiRet`]; the IPI call leaves
/// `value` `0` and reports an invalid mask through `error`.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
#[must_use]
pub fn send_ipi(hart_mask: usize, hart_mask_base: usize) -> SbiRet {
    sbi_call2(SBI_EXT_IPI, SBI_FID_SEND_IPI, hart_mask, hart_mask_base)
}

/// Start the secondary hart `hartid` at `start_addr` via the SBI v0.2
/// HSM extension.
///
/// On success the target hart begins executing in S-mode at
/// `start_addr` with `a0 = hartid` and `a1 = opaque` (the SBI HSM
/// hand-off convention). Returns the [`SbiRet`]; the caller inspects
/// [`SbiRet::is_success`] and fails closed on error rather than assuming the hart came up.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
#[must_use]
pub fn hart_start(hartid: CpuId, start_addr: usize, opaque: usize) -> SbiRet {
    sbi_call3(
        SBI_EXT_HSM,
        SBI_FID_HART_START,
        hartid as usize,
        start_addr,
        opaque,
    )
}

/// Instruct every hart selected by `(hart_mask, hart_mask_base)` to
/// execute an `sfence.vma` covering `[start_addr, start_addr + size)`
/// via the SBI v0.2 RFENCE extension — the riscv64 cross-CPU TLB
/// shootdown.
///
/// riscv64 has no broadcast `sfence.vma`, so the cross-CPU invalidation
/// is delegated to the firmware: `remote_sfence_vma` returns only once
/// the listed harts have fenced, so the call *is* the remote acknowledge
/// — no software ack loop is needed (unlike the x86_64 IPI path). Build
/// the mask for a single hart with [`hart_mask_for`]. The calling hart
/// is **not** covered by the remote fence and must `sfence.vma` itself
/// separately. Returns the [`SbiRet`]; an invalid mask is reported
/// through `error` and dropped by the caller (over-/under-fencing the
/// *remote* set cannot corrupt the local mapping).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
#[must_use]
pub fn remote_sfence_vma(
    hart_mask: usize,
    hart_mask_base: usize,
    start_addr: usize,
    size: usize,
) -> SbiRet {
    sbi_call4(
        SBI_EXT_RFENCE,
        SBI_FID_REMOTE_SFENCE_VMA,
        hart_mask,
        hart_mask_base,
        start_addr,
        size,
    )
}

/// Request a whole-platform reset via the SBI v0.2 System Reset (SRST)
/// extension: `reset_type` selects [`SBI_SRST_TYPE_SHUTDOWN`] (power off)
/// or [`SBI_SRST_TYPE_COLD_REBOOT`] (restart), and `reset_reason` is
/// [`SBI_SRST_REASON_NONE`] for an operator-requested reset.
///
/// On success the firmware powers the platform down or resets it and the
/// call **does not return**; a return therefore always carries a failure
/// [`SbiRet`] (the SRST extension is absent, or the firmware refused the
/// requested type), which the caller reports and recovers from rather than
/// assuming the machine stopped.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
#[must_use]
pub fn system_reset(reset_type: u32, reset_reason: u32) -> SbiRet {
    sbi_call2(
        SBI_EXT_SRST,
        SBI_FID_SYSTEM_RESET,
        reset_type as usize,
        reset_reason as usize,
    )
}

/// Issue an SBI v0.2 `ecall` with two argument registers (`a0`, `a1`).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
fn sbi_call2(eid: usize, fid: usize, arg0: usize, arg1: usize) -> SbiRet {
    let error: isize;
    let value: isize;
    // SAFETY: the SBI v0.2 calling convention places the extension id in
    // `a7`, the function id in `a6`, the arguments in `a0`/`a1`, and
    // returns `error` in `a0` and `value` in `a1`. The call reads only
    // the named input registers, writes no guest memory for the IPI /
    // HSM functions used here, and clobbers only the SBI return
    // registers, which are bound as `inout`/`lateout`.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") eid,
            in("a6") fid,
            inout("a0") arg0 => error,
            inout("a1") arg1 => value,
            options(nostack),
        );
    }
    SbiRet { error, value }
}

/// Issue an SBI v0.2 `ecall` with three argument registers (`a0`–`a2`).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
fn sbi_call3(eid: usize, fid: usize, arg0: usize, arg1: usize, arg2: usize) -> SbiRet {
    let error: isize;
    let value: isize;
    // SAFETY: identical convention to `sbi_call2`, with the third
    // argument supplied in `a2`. `a2` is read-only to the firmware for
    // `hart_start`, so it is bound as a plain `in`.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") eid,
            in("a6") fid,
            inout("a0") arg0 => error,
            inout("a1") arg1 => value,
            in("a2") arg2,
            options(nostack),
        );
    }
    SbiRet { error, value }
}

/// Issue an SBI v0.2 `ecall` with four argument registers (`a0`–`a3`).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
fn sbi_call4(eid: usize, fid: usize, arg0: usize, arg1: usize, arg2: usize, arg3: usize) -> SbiRet {
    let error: isize;
    let value: isize;
    // SAFETY: identical convention to `sbi_call2`, with the third and
    // fourth arguments supplied in `a2`/`a3`. For `remote_sfence_vma`
    // they are the start address and size — read-only to the firmware —
    // so they are bound as plain `in`. The call writes no guest memory
    // and clobbers only the SBI return registers, bound `inout`.
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") eid,
            in("a6") fid,
            inout("a0") arg0 => error,
            inout("a1") arg1 => value,
            in("a2") arg2,
            in("a3") arg3,
            options(nostack),
        );
    }
    SbiRet { error, value }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_ids_match_ascii_encoding() {
        // The SBI specification assigns the IPI, HSM, and RFENCE
        // extension ids the ASCII bytes of "sPI", "HSM", and "RFNC".
        assert_eq!(SBI_EXT_IPI, 0x73_5049);
        assert_eq!(SBI_EXT_HSM, 0x48_534D);
        assert_eq!(SBI_EXT_RFENCE, 0x5246_4E43);
        assert_eq!(SBI_FID_SEND_IPI, 0);
        assert_eq!(SBI_FID_HART_START, 0);
        // `remote_sfence_vma` is function id 1 in the RFENCE extension
        // (function id 0 is `remote_fence_i`).
        assert_eq!(SBI_FID_REMOTE_SFENCE_VMA, 1);
        // The System-Reset extension id is the ASCII bytes of "SRST",
        // `system_reset` is its function id 0, and the two reset types the
        // Supervisor drives are shutdown (0) and cold reboot (1).
        assert_eq!(SBI_EXT_SRST, 0x5352_5354);
        assert_eq!(SBI_FID_SYSTEM_RESET, 0);
        assert_eq!(SBI_SRST_TYPE_SHUTDOWN, 0);
        assert_eq!(SBI_SRST_TYPE_COLD_REBOOT, 1);
        assert_eq!(SBI_SRST_REASON_NONE, 0);
    }

    #[test]
    fn hart_mask_addresses_a_single_hart_at_its_own_base() {
        // A single set bit based at the hart id selects exactly that
        // hart, with no shift that could overflow for a high id.
        assert_eq!(hart_mask_for(0), (1, 0));
        assert_eq!(hart_mask_for(1), (1, 1));
        assert_eq!(hart_mask_for(u32::MAX), (1, u32::MAX as usize));
    }

    #[test]
    fn sbiret_success_predicate() {
        assert!(SbiRet {
            error: SbiRet::SUCCESS,
            value: 0
        }
        .is_success());
        assert!(!SbiRet {
            error: -3,
            value: 0
        }
        .is_success());
    }
}
