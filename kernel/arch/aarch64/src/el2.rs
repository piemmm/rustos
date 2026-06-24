//! Known EL2→EL1 hand-off register values.
//!
//! Every EL2 control register is architecturally **UNKNOWN** when the
//! boot trampoline first runs at EL2 on real silicon: the Pi 4 firmware
//! stub establishes only `SCTLR_EL2` and `CPUECTLR_EL1.SMPEN` before
//! entering the kernel, and QEMU's benign all-zero resets mask any
//! residue under emulation. An UNKNOWN `HCR_EL2.TVM` bit traps EL1's
//! first `MAIR_EL1`/`TCR_EL1`/`TTBR0_EL1`/`SCTLR_EL1` write into
//! vector-less EL2 — on the Pi 4B this hung the boot silently at the
//! exact instant `AddressSpace::switch` ran, while QEMU stayed green.
//! The same reasoning that fixed the UNKNOWN
//! `SCTLR_EL1` reset state ([`crate::paging::SCTLR_MMU_OFF`]) therefore
//! applies one level up: `boot.s`'s EL2 path writes each register
//! **whole** with the values pinned here, never OR-ing into the live
//! register (fail closed, not "trust the reset
//! state").
//!
//! The constants are the single Rust source of truth for the values the
//! trampoline hard-codes; the unit tests pin both the exact encodings
//! and the absence of every trap/booby-trap bit.

/// `HCR_EL2` hand-off value: `RW` (bit 31) only — EL1 executes AArch64;
/// stage-2 translation (`VM`), the EL1 trap controls (`TVM`, `TRVM`,
/// `TSC`, `TWI`, `TWE`, …), `TGE`, and the default-cacheability override
/// (`DC`) are all zero, so EL1 owns its virtual-memory controls and
/// never traps to (vector-less) EL2.
pub const HCR_EL2_HANDOFF: u64 = 1 << 31;

/// `CNTHCTL_EL2` hand-off value: `EL1PCTEN | EL1PCEN` (bits 0–1) — EL1
/// and EL0 read the physical counter and program the physical timer
/// without trapping to EL2; every other (event-stream/trap) field is
/// zero.
pub const CNTHCTL_EL2_HANDOFF: u64 = 0b11;

/// `CPTR_EL2` hand-off value: exactly the ARMv8.0 RES1 bits (`[13:12]`
/// and `[9:0]`) — `TFP` (bit 10, FP/SIMD trap) and `TCPAC` (bit 31,
/// `CPACR_EL1` trap) are clear, so EL1's `enable_fp_el1` and every NEON
/// instruction the compiler emits run untrapped.
pub const CPTR_EL2_HANDOFF: u64 = 0x33FF;

/// `MDCR_EL2` hand-off value: zero — no debug, breakpoint, or PMU
/// register access from EL1/EL0 traps to EL2.
pub const MDCR_EL2_HANDOFF: u64 = 0;

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the exact encodings `boot.s` hard-codes (`mov`/`movk`
    /// immediates), so the trampoline and this module cannot drift
    /// apart silently.
    #[test]
    fn handoff_values_match_the_trampoline_immediates() {
        assert_eq!(HCR_EL2_HANDOFF, 0x8000_0000);
        assert_eq!(CNTHCTL_EL2_HANDOFF, 0x3);
        assert_eq!(CPTR_EL2_HANDOFF, 0x33FF);
        assert_eq!(MDCR_EL2_HANDOFF, 0);
    }

    /// The `HCR_EL2` booby-trap bits whose UNKNOWN reset state hangs or
    /// corrupts EL1 must all be clear, and `RW` must be set.
    #[test]
    fn hcr_handoff_clears_every_el1_trap_and_enables_aarch64() {
        const VM: u64 = 1 << 0; // stage-2 translation
        const FMO_IMO_AMO: u64 = (1 << 3) | (1 << 4) | (1 << 5);
        const DC: u64 = 1 << 12; // default-cacheability override
        const TWI_TWE: u64 = (1 << 13) | (1 << 14);
        const TSC: u64 = 1 << 19; // trap SMC
        const TVM: u64 = 1 << 26; // trap EL1 virtual-memory controls
        const TGE: u64 = 1 << 27; // trap general exceptions
        const TRVM: u64 = 1 << 30; // trap EL1 VM-control *reads*
        const RW: u64 = 1 << 31; // EL1 is AArch64

        let traps = VM | FMO_IMO_AMO | DC | TWI_TWE | TSC | TVM | TGE | TRVM;
        assert_eq!(HCR_EL2_HANDOFF & traps, 0);
        assert_eq!(HCR_EL2_HANDOFF & RW, RW);
    }

    /// `CNTHCTL_EL2` must grant EL1/EL0 the physical counter and timer.
    #[test]
    fn cnthctl_handoff_grants_counter_and_timer() {
        const EL1PCTEN: u64 = 1 << 0;
        const EL1PCEN: u64 = 1 << 1;
        assert_eq!(
            CNTHCTL_EL2_HANDOFF & (EL1PCTEN | EL1PCEN),
            EL1PCTEN | EL1PCEN
        );
    }

    /// `CPTR_EL2` must be the RES1 pattern with both trap bits clear.
    #[test]
    fn cptr_handoff_is_res1_with_no_traps() {
        const RES1: u64 = (0b11 << 12) | 0x3FF;
        const TFP: u64 = 1 << 10;
        const TCPAC: u64 = 1 << 31;
        assert_eq!(CPTR_EL2_HANDOFF, RES1);
        assert_eq!(CPTR_EL2_HANDOFF & (TFP | TCPAC), 0);
    }
}
