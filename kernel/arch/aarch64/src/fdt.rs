//! aarch64 device-tree access.
//!
//! The flattened-device-tree parser itself is architecture-neutral and
//! lives once in [`rustos_fdt`] (`AGENTS.md` §2.2 — no duplication); this
//! module re-exports it and layers the aarch64-specific *queries* the boot
//! path needs from the `virt` board's device tree:
//!
//! * the first `/memory` region (delegated to
//!   [`rustos_fdt::Fdt::first_memory_region`]);
//! * the `/psci` `method` — the conduit (`hvc`/`smc`) the kernel uses to
//!   call PSCI firmware for secondary-core bring-up (the prerequisite for
//!   aarch64 SMP, `plans/WIRING.md` Stage W6);
//! * the optional generic-timer `clock-frequency` counter-rate override
//!   the `/timer` node may carry (`plans/PI.md` P4).
//!
//! Devices reach [`rustos_abi::hwtree`] through the *generic* walk in
//! [`crate::platform`] — no per-device query is needed for a node to be
//! discovered and matched (`AGENTS.md` §18.2/§18.3).

pub use rustos_fdt::{Fdt, FdtError};

/// The PSCI conduit a platform uses to call firmware (`AGENTS.md` §11 /
/// `plans/WIRING.md` W6).
///
/// Discovered from the `/psci` node's `method` property; selects the
/// instruction (`hvc` at EL2-hosted, `smc` at EL3-hosted) the secondary
/// bring-up path issues.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PsciMethod {
    /// Hypervisor call — the firmware lives at EL2 (QEMU `virt` default).
    Hvc,
    /// Secure-monitor call — the firmware lives at EL3.
    Smc,
}

impl PsciMethod {
    /// Parse a `/psci` `method` property value (a NUL-terminated string).
    #[must_use]
    pub fn from_property(value: &[u8]) -> Option<Self> {
        // Trim a trailing NUL the device tree stores on string properties.
        let s = match value.iter().position(|&b| b == 0) {
            Some(nul) => &value[..nul],
            None => value,
        };
        match s {
            b"hvc" => Some(Self::Hvc),
            b"smc" => Some(Self::Smc),
            _ => None,
        }
    }
}

/// Read the PSCI conduit from the `/psci` node, or `None` if the tree
/// declares no PSCI node or an unrecognised method.
///
/// Matches the `/psci` node through the shared `Fdt::nodes` early-return
/// walk — the same byte-safe traversal [`crate::gic::configure_from_fdt`]
/// and [`timer_clock_frequency`] use (`AGENTS.md` §2.2) — and reads
/// `method` from that node's own properties. It stops at the first
/// matching node and never scans the whole tree, so it stays safe with
/// the MMU off (the secondary-bring-up boot path discovers the conduit
/// before the page tables are live; a whole-tree `Fdt::property`/`walk`
/// scan faults there once the compiler widens the byte reads —
/// `plans/PI.md` P4/P5 watch-out).
#[must_use]
pub fn psci_method(fdt: &Fdt<'_>) -> Option<PsciMethod> {
    let node = fdt.nodes().flatten().find(node_is_psci)?;
    PsciMethod::from_property(node.property("method")?.value())
}

/// `true` iff `node` is the standard `/psci` node — matched by an
/// `arm,psci` `compatible` prefix (covering `arm,psci`, `arm,psci-0.2`,
/// `arm,psci-1.0`, …) so the conduit is read from the firmware's PSCI
/// node regardless of the exact revision it advertises.
fn node_is_psci(node: &rustos_fdt::Node<'_>) -> bool {
    node.property("compatible").is_some_and(|compatible| {
        compatible
            .iter_strings()
            .any(|s| s.starts_with(b"arm,psci"))
    })
}

/// Read the generic-timer counter frequency (Hz) the `/timer` node
/// declares through its `clock-frequency` property, if any.
///
/// `clock-frequency` is the standard `arm,armv?-timer` override the
/// device tree carries when firmware leaves `CNTFRQ_EL0` mis-programmed
/// (the Linux binding honours it ahead of the register). The canonical
/// source is still the `CNTFRQ_EL0` register (`kernel_arch::read_cntfrq`);
/// this surfaces only the optional tree override so the boot path can
/// prefer it. Returns `None` when the node omits the property or the
/// value is not a single big-endian `u32`.
#[must_use]
pub fn timer_clock_frequency(fdt: &Fdt<'_>) -> Option<u32> {
    // Match the generic-timer node by its `compatible` string and read
    // `clock-frequency` from that node's own properties, via the shared
    // `Fdt::nodes` walk — the same early-returning traversal
    // [`crate::gic::configure_from_fdt`] uses (`AGENTS.md` §2.2). This
    // stops at the first matching node and only ever reads that node's
    // properties, never scanning the whole tree.
    let node = fdt
        .nodes()
        .flatten()
        .find(|node| node.is_compatible("arm,armv8-timer"))?;
    let value = node.property("clock-frequency")?.value();
    let bytes = value.get(0..4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Select the generic-timer frequency (Hz) to drive preemption with,
/// preferring the device-tree `clock-frequency` override
/// ([`timer_clock_frequency`]) when present and non-zero, otherwise the
/// `CNTFRQ_EL0` register value `cntfrq`.
///
/// A zero `clock-frequency` is treated as absent (a board that declares
/// `0` has not really told us a rate), so the register value is used
/// rather than a divide-by-zero timer interval (`AGENTS.md` §2.9 — fail
/// closed). Pure and host-testable: the register read is the caller's
/// responsibility (`kernel_arch::timer_frequency_hz` composes the two on
/// the freestanding target).
#[must_use]
pub const fn effective_timer_hz(tree_clock_frequency: Option<u32>, cntfrq: u64) -> u64 {
    match tree_clock_frequency {
        Some(hz) if hz != 0 => hz as u64,
        _ => cntfrq,
    }
}

#[cfg(test)]
mod tests {
    use super::{effective_timer_hz, psci_method, timer_clock_frequency, Fdt, PsciMethod};
    use rustos_fdt::fixture::{raspi_like_arm, virt_like_arm, DtbBuilder};

    /// Build a minimal tree carrying a `/timer` node with the given
    /// `clock-frequency` (a single big-endian `u32`), used to exercise
    /// the P4 counter-rate override reader.
    fn tree_with_timer_clock(hz: u32) -> std::vec::Vec<u8> {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("timer");
        b.prop_str("compatible", "arm,armv8-timer");
        b.prop_u32("clock-frequency", hz);
        b.end_node();
        b.end_node();
        b.build()
    }

    #[test]
    fn reads_psci_method_hvc_and_smc() {
        let hvc = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 14);
        let fdt = Fdt::new(&hvc).expect("valid fdt");
        assert_eq!(psci_method(&fdt), Some(PsciMethod::Hvc));

        let smc = virt_like_arm(0x4000_0000, 0x2000_0000, "smc", 14);
        let fdt = Fdt::new(&smc).expect("valid fdt");
        assert_eq!(psci_method(&fdt), Some(PsciMethod::Smc));
    }

    #[test]
    fn reads_psci_method_from_a_raspi_shaped_tree() {
        // The Raspberry-Pi-shaped fixture carries a `/psci` node with the
        // `smc` conduit an EL3-firmware platform uses (`armstub8.bin`).
        let blob = raspi_like_arm(0xfe20_1000, 0xfe21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(psci_method(&fdt), Some(PsciMethod::Smc));
    }

    #[test]
    fn psci_method_absent_on_a_tree_without_the_node() {
        // A tree with no `/psci` node yields `None`, so the bring-up path
        // fails closed rather than assuming a conduit (`AGENTS.md` §5.4.5).
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("timer");
        b.prop_str("compatible", "arm,armv8-timer");
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(psci_method(&fdt), None);
    }

    #[test]
    fn unknown_psci_method_is_rejected() {
        assert_eq!(PsciMethod::from_property(b"nonsense\0"), None);
        assert_eq!(PsciMethod::from_property(b"hvc\0"), Some(PsciMethod::Hvc));
        assert_eq!(PsciMethod::from_property(b"smc"), Some(PsciMethod::Smc));
    }

    #[test]
    fn reads_timer_clock_frequency_when_the_node_declares_it() {
        // The Pi 4's 54 MHz crystal is the evocative value here.
        let blob = tree_with_timer_clock(54_000_000);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(timer_clock_frequency(&fdt), Some(54_000_000));
    }

    #[test]
    fn timer_clock_frequency_absent_on_a_tree_without_the_property() {
        // The `virt`-shaped fixture carries `/timer` `interrupts` but no
        // `clock-frequency`, so the override is absent and the boot path
        // falls back to `CNTFRQ_EL0`.
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 30);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(timer_clock_frequency(&fdt), None);
    }

    #[test]
    fn effective_timer_hz_prefers_a_nonzero_tree_override() {
        // A present, non-zero tree value wins over the register reading.
        assert_eq!(effective_timer_hz(Some(54_000_000), 62_500_000), 54_000_000);
    }

    #[test]
    fn effective_timer_hz_falls_back_to_the_register() {
        // Absent override → the `CNTFRQ_EL0` value is used.
        assert_eq!(effective_timer_hz(None, 62_500_000), 62_500_000);
        // A zero override is treated as absent (never a 0 Hz timer).
        assert_eq!(effective_timer_hz(Some(0), 62_500_000), 62_500_000);
    }
}
