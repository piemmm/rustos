//! Boot-CPU model-name discovery for aarch64.
//!
//! Arm silicon identifies itself through `MIDR_EL1` (Arm ARM DDI 0487,
//! "Main ID Register"): an implementer code in bits `[31:24]` and a
//! primary part number in bits `[15:4]`. This module decodes that pair
//! into the part's marketing name (`ARM Cortex-A72`) for the boot facts.
//!
//! The decoder (`name_for_midr`) is pure and host-testable; the register
//! read only executes on the bare-metal target and the host build reports
//! `None` (no fake hardware in production paths). An implementer/part
//! pair outside the table is `None` — an honest "unknown" the boot facts
//! record as such — never a guessed name.

/// The implementer code (`MIDR_EL1[31:24]`) of Arm Ltd designed parts.
const IMPLEMENTER_ARM: u64 = 0x41;

/// Marketing names of Arm Ltd parts by primary part number
/// (`MIDR_EL1[15:4]`), per each core's Technical Reference Manual.
///
/// The table carries the Cortex-A and Neoverse parts TAIRiX's Tier-1
/// aarch64 hardware actually presents (the Raspberry Pi 3/4/5 cores and
/// the cores QEMU's `virt` board models) plus their common siblings; an
/// unlisted part reports `None` and the boot facts stay honest.
const ARM_PARTS: &[(u64, &str)] = &[
    (0xD03, "ARM Cortex-A53"),
    (0xD04, "ARM Cortex-A35"),
    (0xD05, "ARM Cortex-A55"),
    (0xD07, "ARM Cortex-A57"),
    (0xD08, "ARM Cortex-A72"),
    (0xD09, "ARM Cortex-A73"),
    (0xD0A, "ARM Cortex-A75"),
    (0xD0B, "ARM Cortex-A76"),
    (0xD0C, "ARM Neoverse N1"),
    (0xD0D, "ARM Cortex-A77"),
    (0xD40, "ARM Neoverse V1"),
    (0xD41, "ARM Cortex-A78"),
    (0xD44, "ARM Cortex-X1"),
    (0xD46, "ARM Cortex-A510"),
    (0xD47, "ARM Cortex-A710"),
    (0xD48, "ARM Cortex-X2"),
    (0xD49, "ARM Neoverse N2"),
    (0xD4D, "ARM Cortex-A715"),
    (0xD4E, "ARM Cortex-X3"),
];

/// Decode a raw `MIDR_EL1` value into the part's marketing name.
///
/// Returns `None` for an implementer or part number outside the table —
/// the caller records an honest "unknown", never a guessed name.
#[must_use]
pub fn name_for_midr(midr: u64) -> Option<&'static str> {
    let implementer = (midr >> 24) & 0xFF;
    if implementer != IMPLEMENTER_ARM {
        return None;
    }
    let part = (midr >> 4) & 0xFFF;
    ARM_PARTS
        .iter()
        .find(|&&(number, _)| number == part)
        .map(|&(_, name)| name)
}

/// The marketing name of the CPU this function executes on, or `None`
/// when it is not in the table.
///
/// On the bare-metal target it reads `MIDR_EL1` and decodes it through
/// [`name_for_midr`]. On the host target there is no EL1 register to
/// read, so it reports `None` rather than the host machine's identity.
#[must_use]
pub fn boot_cpu_name() -> Option<&'static str> {
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    {
        let midr: u64;
        // SAFETY: `MIDR_EL1` is readable at EL1 (the boot path drops any
        // EL2 entry to EL1 before the kernel runs) and the read has no
        // architectural side effect.
        unsafe {
            core::arch::asm!("mrs {midr}, midr_el1", midr = out(reg) midr, options(nomem, nostack, preserves_flags));
        }
        name_for_midr(midr)
    }
    #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_arm_parts_decode_to_their_names() {
        // The Raspberry Pi 3/4/5 cores, from each core's TRM.
        assert_eq!(name_for_midr(0x410F_D034), Some("ARM Cortex-A53"));
        assert_eq!(name_for_midr(0x410F_D083), Some("ARM Cortex-A72"));
        assert_eq!(name_for_midr(0x414F_D0B1), Some("ARM Cortex-A76"));
        // Variant/revision bits never affect the decode.
        assert_eq!(name_for_midr(0x412F_D083), Some("ARM Cortex-A72"));
    }

    #[test]
    fn unknown_implementer_or_part_is_none() {
        // A non-Arm implementer (Apple, 0x61) with an Arm part number.
        assert_eq!(name_for_midr(0x610F_D083), None);
        // An Arm implementer with a part number outside the table.
        assert_eq!(name_for_midr(0x410F_FFF0), None);
        assert_eq!(name_for_midr(0), None);
    }

    #[test]
    fn host_read_reports_none() {
        // The host build has no EL1 register to read and must not report
        // the host machine's identity.
        assert_eq!(boot_cpu_name(), None);
    }
}
