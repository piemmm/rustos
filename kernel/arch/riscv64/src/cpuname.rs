//! Boot-CPU model-name discovery for riscv64.
//!
//! RISC-V S-mode software cannot read the machine-mode identity CSRs
//! (`mvendorid`/`marchid`), so the CPU model comes from the same source
//! every other riscv64 discovery uses: the device tree. Each
//! `/cpus/cpu@*` node names its core through its `compatible` property
//! (Devicetree Specification; the RISC-V cpus binding), e.g.
//! `sifive,u74-mc` on a SiFive part — read by the boot path through
//! [`rustos_fdt::Fdt::boot_cpu_compatible`] and mapped here.
//!
//! The mapper is pure and host-testable. A compatible outside the table —
//! including the bare `riscv` QEMU's `virt` board advertises, which names
//! the ISA rather than a part — is `None`: an honest "unknown" the boot
//! facts record as such, never a guessed name.

/// Marketing names by device-tree `compatible` string, per each vendor's
/// published core naming.
const RISCV_PARTS: &[(&str, &str)] = &[
    ("sifive,e51", "SiFive E51"),
    ("sifive,s76", "SiFive S76"),
    ("sifive,u54", "SiFive U54"),
    ("sifive,u54-mc", "SiFive U54-MC"),
    ("sifive,u74", "SiFive U74"),
    ("sifive,u74-mc", "SiFive U74-MC"),
    ("thead,c906", "T-Head C906"),
    ("thead,c910", "T-Head C910"),
    ("thead,c920", "T-Head C920"),
];

/// Map a cpu node's first `compatible` string to the part's marketing
/// name.
///
/// Returns `None` for a compatible outside the table — the caller
/// records an honest "unknown", never a guessed name.
#[must_use]
pub fn name_for_compatible(compatible: &str) -> Option<&'static str> {
    RISCV_PARTS
        .iter()
        .find(|&&(key, _)| key == compatible)
        .map(|&(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_compatibles_map_to_their_names() {
        assert_eq!(name_for_compatible("sifive,u74-mc"), Some("SiFive U74-MC"));
        assert_eq!(name_for_compatible("thead,c906"), Some("T-Head C906"));
    }

    #[test]
    fn unknown_or_generic_compatibles_are_none() {
        // QEMU's `virt` board names only the ISA, not a part.
        assert_eq!(name_for_compatible("riscv"), None);
        assert_eq!(name_for_compatible(""), None);
        assert_eq!(name_for_compatible("acme,warpcore"), None);
    }
}
