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
//! * the generic-timer per-CPU interrupt (PPI) number from `/timer`.
//!
//! The normalisation of these facts into [`rustos_abi::hwtree`] nodes lives
//! in [`crate::platform`].

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
#[must_use]
pub fn psci_method(fdt: &Fdt<'_>) -> Option<PsciMethod> {
    PsciMethod::from_property(fdt.property(&[b"psci"], b"method")?)
}

/// Read the generic-timer interrupt (PPI) number from the `/timer` node.
///
/// The `interrupts` property is a list of GIC specifier triples
/// `<type, number, flags>`; this returns the `number` cell of the first
/// specifier. Selecting *which* of the (secure / non-secure physical /
/// virtual / hypervisor) timers to arm is a Stage W6 refinement; W1 only
/// needs to surface that a timer PPI exists.
#[must_use]
pub fn timer_ppi(fdt: &Fdt<'_>) -> Option<u32> {
    let interrupts = fdt.property(&[b"timer"], b"interrupts")?;
    // Triple layout: type @0, number @4, flags @8 (each a big-endian u32).
    let number = interrupts.get(4..8)?;
    Some(u32::from_be_bytes([
        number[0], number[1], number[2], number[3],
    ]))
}

#[cfg(test)]
mod tests {
    use super::{psci_method, timer_ppi, Fdt, PsciMethod};
    use rustos_fdt::fixture::virt_like_arm;

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
    fn unknown_psci_method_is_rejected() {
        assert_eq!(PsciMethod::from_property(b"nonsense\0"), None);
        assert_eq!(PsciMethod::from_property(b"hvc\0"), Some(PsciMethod::Hvc));
        assert_eq!(PsciMethod::from_property(b"smc"), Some(PsciMethod::Smc));
    }

    #[test]
    fn reads_timer_ppi_and_memory() {
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 30);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(timer_ppi(&fdt), Some(30));
        assert_eq!(fdt.first_memory_region(), Some((0x4000_0000, 0x2000_0000)));
    }
}
