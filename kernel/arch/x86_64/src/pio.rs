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

use rustos_abi::driver::port_io::{PortIo, PortIo8};

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

/// Zero-sized x86_64 8-bit port-I/O backend.
///
/// The byte-wide sibling of [`X86PortIo`]: it implements the
/// [`PortIo8`] seam (`lib/abi`) for byte-addressed legacy register
/// files such as the Intel 8042 keyboard controller the
/// `drivers/input/ps2` driver drives (status/command port `0x64`,
/// data port `0x60`). Like [`X86PortIo`] it carries no state — it
/// names the global architectural I/O port space — so constructing
/// one issues no I/O and is sound to do before any device is probed.
#[derive(Copy, Clone, Debug, Default)]
pub struct X86PortIo8;

/// Construct the x86_64 8-bit port-I/O backend.
///
/// The result is handed to a byte-addressed legacy driver (today the
/// `drivers/input/ps2` i8042 keyboard driver) so it can issue 8-bit
/// reads and writes through the [`PortIo8`] seam without depending on
/// this crate.
#[must_use]
pub const fn x86_port_io8() -> X86PortIo8 {
    X86PortIo8
}

impl PortIo8 for X86PortIo8 {
    fn read8(&self, port: u16) -> u8 {
        #[cfg(target_arch = "x86_64")]
        {
            let value: u8;
            // SAFETY: `in al, dx` is a side-effect-only 8-bit PIO read
            // against `port`. Callers reach this backend only through
            // the `PortIo8` seam and drive byte-addressed legacy
            // register files (the i8042 controller's `0x60`/`0x64`); an
            // 8-bit read touches exactly the addressed register and
            // aliases no neighbour. The instruction has no memory side
            // effects and clobbers no register outside `al`, so the
            // conservative `nomem`, `nostack`, and `preserves_flags`
            // options hold. The surrounding `cfg(target_arch =
            // "x86_64")` guarantees `in`/`out` are only emitted for an
            // x86_64 code generator.
            unsafe {
                core::arch::asm!(
                    "in al, dx",
                    in("dx") port,
                    out("al") value,
                    options(nomem, nostack, preserves_flags),
                );
            }
            value
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            // Non-x86_64 host build: the legacy port I/O space exists
            // only on x86, so `in`/`out` have no encoding here. This
            // backend is never reached on such hosts (the i8042 driver's
            // host unit tests use a mock `PortIo8`), so the shim returns
            // a constant rather than emitting an invalid instruction;
            // returning a value honours `AGENTS.md` §2.9.
            let _ = port;
            0
        }
    }

    fn write8(&self, port: u16, value: u8) {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: `out dx, al` is a side-effect-only 8-bit PIO write
            // to `port`. Same justification as `read8`: callers drive
            // only byte-addressed legacy controller registers through
            // the `PortIo8` seam, the instruction touches no memory, and
            // the assembler template declares the conservative `nomem`,
            // `nostack`, and `preserves_flags` options.
            unsafe {
                core::arch::asm!(
                    "out dx, al",
                    in("dx") port,
                    in("al") value,
                    options(nomem, nostack, preserves_flags),
                );
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            // Non-x86_64 host build: see `read8`. No port space exists
            // off x86, and this backend is never reached on such hosts.
            let _ = (port, value);
        }
    }
}

impl PortIo for X86PortIo {
    fn read32(&self, port: u16) -> u32 {
        #[cfg(target_arch = "x86_64")]
        {
            let value: u32;
            // SAFETY: `in eax, dx` is a side-effect-only 32-bit PIO read
            // against `port`. The sole caller (the PCI bus driver's
            // mechanism-#1 bridge) only ever passes the legacy PCI
            // configuration ports `0xCF8`/`0xCFC`, documented as 32-bit
            // I/O ports by the PCI Local Bus 3.0 specification
            // §3.2.2.3.2. The instruction has no memory side effects and
            // clobbers no registers outside `eax`; the conservative
            // `nomem`, `nostack`, and `preserves_flags` options are
            // declared accordingly. The surrounding `cfg(target_arch =
            // "x86_64")` guarantees `in`/`out` are only emitted for an
            // x86_64 code generator.
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
        #[cfg(not(target_arch = "x86_64"))]
        {
            // Non-x86_64 host build: the legacy port I/O space exists
            // only on x86. The PCI bus driver's host unit tests use a
            // mock `PortIo`, so this backend is never reached; the shim
            // returns a constant rather than emitting an invalid
            // instruction (`AGENTS.md` §2.9).
            let _ = port;
            0
        }
    }

    fn write32(&self, port: u16, value: u32) {
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: `out dx, eax` is a side-effect-only 32-bit PIO
            // write to `port`. Same justification as `read32`: only the
            // documented PCI configuration ports are ever passed, the
            // instruction touches no memory, and the assembler template
            // declares the conservative `nomem`, `nostack`, and
            // `preserves_flags` options.
            unsafe {
                core::arch::asm!(
                    "out dx, eax",
                    in("dx") port,
                    in("eax") value,
                    options(nomem, nostack, preserves_flags),
                );
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            // Non-x86_64 host build: see `read32`. No port space exists
            // off x86, and this backend is never reached on such hosts.
            let _ = (port, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accepts the backend only through the `&dyn PortIo` seam, proving
    /// the coercion the bus driver relies on compiles.
    fn accepts_seam(_seam: &dyn PortIo) {}

    /// Accepts the byte-wide backend only through the `&dyn PortIo8`
    /// seam, proving the coercion the ps2 driver relies on compiles.
    fn accepts_seam8(_seam: &dyn PortIo8) {}

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

    /// The byte-wide backend is likewise a zero-sized handle and
    /// coerces to the `&dyn PortIo8` seam the ps2 driver consumes. The
    /// `in`/`out` instructions are privileged and cannot run from a
    /// host unit test, so this asserts only construction and the
    /// trait-object coercion; the read/write behaviour against the
    /// i8042 ports is covered against a mock backend in
    /// `drivers/input/ps2` and end-to-end in the
    /// `ps2_input_qemu_x86_64` QEMU vertical.
    #[test]
    fn byte_backend_is_zero_sized_and_coerces_to_the_seam() {
        assert_eq!(core::mem::size_of::<X86PortIo8>(), 0);
        let backend = x86_port_io8();
        accepts_seam8(&backend);
    }
}
