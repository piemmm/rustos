//! PSCI (Power State Coordination Interface) firmware calls.
//!
//! Secondary-core bring-up on the ARMv8-A `virt` board goes through PSCI:
//! the boot core asks firmware to power on a parked core with the
//! `CPU_ON` function, handing it an entry point and an opaque context
//! value (`plans/WIRING.md` Stage W6). This module is the aarch64
//! analogue of riscv64's `sbi` module: it exposes only the small,
//! fixed set of PSCI services the bring-up path needs (
//! — no bloat).
//!
//! # Conduit
//!
//! PSCI is invoked through a *conduit* — an `hvc` (hypervisor call) or
//! `smc` (secure-monitor call) instruction, selected by the platform's
//! `/psci` `method` property and decoded into a [`crate::fdt::PsciMethod`]
//! by the device-tree reader. The QEMU `virt` board emulates PSCI behind
//! `hvc`; an EL3-firmware platform uses `smc`. The two differ only by the
//! trap instruction, so `cpu_on` dispatches on the method.
//!
//! # Return values
//!
//! Every PSCI call returns a signed status in `x0`: `0`
//! (`SUCCESS`) on success, a negative `error` code otherwise. The
//! bring-up path inspects `PsciRet::is_success` and fails closed rather than assuming the core came up.
//!
//! # Host testability
//!
//! The function id, the SMC64 calling-convention encoding, and the
//! status decode are pure values, unit-tested on the host; only the
//! `hvc`/`smc` instruction is gated to the freestanding aarch64 target.

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
use crate::fdt::PsciMethod;

/// PSCI `CPU_ON` function id (SMC64 calling convention
/// / PSCI spec §5.1.3). The bit-30 SMC64 flag (`0x4000_0000`) selects
/// the 64-bit argument convention, so `target_cpu`, `entry_point`, and
/// `context_id` are passed as full 64-bit values in `x1`/`x2`/`x3`.
pub const PSCI_CPU_ON: u32 = 0xC400_0003;

/// PSCI status codes (PSCI spec table 5-3). Success is `0`; every defined
/// error is negative.
pub mod error {
    /// The call succeeded.
    pub const SUCCESS: i32 = 0;
    /// The function is not implemented by the firmware.
    pub const NOT_SUPPORTED: i32 = -1;
    /// A parameter (function id, MPIDR, …) was invalid.
    pub const INVALID_PARAMETERS: i32 = -2;
    /// The call was refused (e.g. the core is being powered off).
    pub const DENIED: i32 = -3;
    /// The target core is already on.
    pub const ALREADY_ON: i32 = -4;
    /// A `CPU_ON` for the target core is already in progress.
    pub const ON_PENDING: i32 = -5;
    /// The firmware hit an internal error.
    pub const INTERNAL_FAILURE: i32 = -6;
    /// The target core is not present in the system.
    pub const NOT_PRESENT: i32 = -7;
    /// The target core is disabled.
    pub const DISABLED: i32 = -8;
    /// The entry-point address was invalid.
    pub const INVALID_ADDRESS: i32 = -9;
}

pub use error::SUCCESS;

/// The signed status a PSCI call returns in `x0`.
///
/// Wrapping the raw value keeps the success predicate and the stable
/// audit cause string in one place, mirroring riscv64's `sbi::SbiRet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PsciRet {
    /// The raw PSCI status code (`0` on success; negative otherwise).
    pub status: i32,
}

impl PsciRet {
    /// `true` iff the call succeeded (`status == SUCCESS`).
    #[must_use]
    pub const fn is_success(self) -> bool {
        self.status == error::SUCCESS
    }

    /// Stable cause string for audit records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self.status {
            error::SUCCESS => "psci_success",
            error::NOT_SUPPORTED => "psci_not_supported",
            error::INVALID_PARAMETERS => "psci_invalid_parameters",
            error::DENIED => "psci_denied",
            error::ALREADY_ON => "psci_already_on",
            error::ON_PENDING => "psci_on_pending",
            error::INTERNAL_FAILURE => "psci_internal_failure",
            error::NOT_PRESENT => "psci_not_present",
            error::DISABLED => "psci_disabled",
            error::INVALID_ADDRESS => "psci_invalid_address",
            _ => "psci_unknown_error",
        }
    }
}

/// Power on the secondary core identified by `target_mpidr`, entering it
/// at `entry_point` with `x0 = context_id`.
///
/// Issues the PSCI [`PSCI_CPU_ON`] call through the conduit `method`
/// names (`hvc` or `smc`). On success the firmware starts the target
/// core in the calling exception level at `entry_point`; the PSCI
/// hand-off convention places `context_id` in the new core's `x0`.
///
/// Returns the [`PsciRet`]; the caller inspects [`PsciRet::is_success`]
/// and fails closed on error.
///
/// # Safety
///
/// `entry_point` must be the physical address of a valid secondary-entry
/// trampoline (with the MMU off, as the freshly-powered core runs), and
/// `target_mpidr` must name a real, parked core distinct from the
/// caller. The call mutates firmware-owned power state and is otherwise
/// side-effect-free to this core.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub unsafe fn cpu_on(
    method: PsciMethod,
    target_mpidr: u64,
    entry_point: u64,
    context_id: u64,
) -> PsciRet {
    let status: i64;
    // The SMCCC passes the function id in `x0` and the arguments in
    // `x1`–`x3`, returns the status in `x0`, and may clobber the
    // result/scratch registers `x1`–`x17` (so `x1`–`x3` are bound as
    // discarded outputs and `x4`–`x17` are listed as clobbers rather
    // than using `clobber_abi`, which would conflict with the explicit
    // `x0`–`x3` operands).
    macro_rules! psci_smccc {
        ($conduit:literal) => {
            core::arch::asm!(
                $conduit,
                inout("x0") u64::from(PSCI_CPU_ON) => status,
                inout("x1") target_mpidr => _,
                inout("x2") entry_point => _,
                inout("x3") context_id => _,
                lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
                lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
                lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
                lateout("x16") _, lateout("x17") _,
                options(nostack),
            )
        };
    }
    match method {
        // SAFETY: `hvc #0` traps to the PSCI implementation (the
        // hypervisor / QEMU `virt` PSCI emulation); `smc #0` traps to
        // EL3 secure-monitor firmware. Both follow the SMCCC convention
        // the macro encodes, touch no guest memory of ours, and only
        // clobber the registers declared above.
        PsciMethod::Hvc => unsafe { psci_smccc!("hvc #0") },
        PsciMethod::Smc => unsafe { psci_smccc!("smc #0") },
    }
    #[allow(clippy::cast_possible_truncation)]
    PsciRet {
        status: status as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_on_function_id_encodes_the_smc64_convention() {
        // Bit 31 (fast call), bit 30 (SMC64), service 0 (PSCI),
        // function number 3 (CPU_ON).
        assert_eq!(PSCI_CPU_ON, 0xC400_0003);
        assert_eq!(PSCI_CPU_ON & (1 << 30), 1 << 30, "SMC64 convention bit");
    }

    #[test]
    fn success_predicate_matches_the_zero_status() {
        assert!(PsciRet { status: SUCCESS }.is_success());
        assert!(!PsciRet {
            status: error::ALREADY_ON
        }
        .is_success());
        assert!(!PsciRet {
            status: error::INVALID_PARAMETERS
        }
        .is_success());
    }

    #[test]
    fn status_cause_strings_are_stable() {
        assert_eq!(PsciRet { status: SUCCESS }.as_str(), "psci_success");
        assert_eq!(
            PsciRet {
                status: error::ALREADY_ON
            }
            .as_str(),
            "psci_already_on"
        );
        assert_eq!(
            PsciRet {
                status: error::NOT_PRESENT
            }
            .as_str(),
            "psci_not_present"
        );
        // An undefined status decodes to the catch-all rather than
        // panicking.
        assert_eq!(PsciRet { status: -42 }.as_str(), "psci_unknown_error");
    }
}
