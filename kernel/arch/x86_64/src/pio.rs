//! x86_64 legacy port-I/O implementation of the Arch HAL port-I/O seam.
//!
//! This is the single in-tree implementor of the
//! [`rustos_abi::PortIo`](rustos_abi::driver::port_io::PortIo) seam
//! (`AGENTS.md` §17.2 / §17.4). It encapsulates the `in`/`out`
//! instructions — the only way to reach the legacy PCI configuration
//! ports `0xCF8`/`0xCFC` (PCI Local Bus 3.0 §3.2.2.3.2) — behind the safe
//! trait so the `drivers/bus/pci` bus driver consumes it without naming
//! this architecture port and without a `cfg(target_arch …)` gate of its
//! own. The bus driver receives an `X86PortIo` by value from the ring-0
//! bring-up path and reaches it only through `&dyn PortIo`.

use rustos_abi::driver::port_io::PortIo;

/// Zero-sized x86_64 port-I/O backend.
///
/// Carries no state — it names the architectural I/O port space, which
/// is global — so constructing one issues no I/O and is sound to do
/// before the PCI host bridge has been probed.
#[derive(Copy, Clone, Debug, Default)]
pub struct X86PortIo;

/// Construct the x86_64 port-I/O backend for the PCI bus driver.
///
/// The result is handed to `rustos_drv_bus_pci::mechanism_one` so the
/// bus driver can issue PCI configuration accesses through the
/// [`PortIo`] seam without depending on this crate.
#[must_use]
pub const fn x86_port_io() -> X86PortIo {
    X86PortIo
}

impl PortIo for X86PortIo {
    fn read32(&self, port: u16) -> u32 {
        let value: u32;
        // SAFETY: `in eax, dx` is a side-effect-only 32-bit PIO read
        // against `port`. The sole caller (the PCI bus driver's
        // mechanism-#1 bridge) only ever passes the legacy PCI
        // configuration ports `0xCF8`/`0xCFC`, documented as 32-bit I/O
        // ports by the PCI Local Bus 3.0 specification §3.2.2.3.2. The
        // instruction has no memory side effects and clobbers no
        // registers outside `eax`; the conservative `nomem`, `nostack`,
        // and `preserves_flags` options are declared accordingly.
        unsafe {
            core::arch::asm!(
                "in eax, dx",
                in("dx") port,
                out("eax") value,
                options(nomem, nostack, preserves_flags),
            );
        }
        value
    }

    fn write32(&self, port: u16, value: u32) {
        // SAFETY: `out dx, eax` is a side-effect-only 32-bit PIO write to
        // `port`. Same justification as `read32`: only the documented PCI
        // configuration ports are ever passed, the instruction touches no
        // memory, and the assembler template declares the conservative
        // `nomem`, `nostack`, and `preserves_flags` options.
        unsafe {
            core::arch::asm!(
                "out dx, eax",
                in("dx") port,
                in("eax") value,
                options(nomem, nostack, preserves_flags),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accepts the backend only through the `&dyn PortIo` seam, proving
    /// the coercion the bus driver relies on compiles.
    fn accepts_seam(_seam: &dyn PortIo) {}

    /// The backend is a zero-sized handle: constructing it is a no-op
    /// and it round-trips through the `&dyn PortIo` seam the bus driver
    /// consumes. The `in`/`out` instructions themselves are privileged
    /// and cannot be exercised from a host unit test, so this asserts
    /// only the construction and the trait-object coercion; the
    /// address/data interleaving is covered against a mock backend in
    /// `drivers/bus/pci`.
    #[test]
    fn backend_is_zero_sized_and_coerces_to_the_seam() {
        assert_eq!(core::mem::size_of::<X86PortIo>(), 0);
        let backend = x86_port_io();
        accepts_seam(&backend);
    }
}
