//! Board-discovered console model and runtime base (`plans/PI.md` P2).
//!
//! The aarch64 port boots on two boards that disagree on *where* and
//! *what* the console UART is. The QEMU `virt` board carries a single
//! PrimeCell PL011 at a fixed low address; the Raspberry Pi (BCM2835 /
//! BCM2711) exposes both a PL011 (`UART0`) and a BCM2835 **AUX mini-UART**
//! at the SoC's high peripheral window, and which one is wired to the
//! physical pins is a board-integration choice. Per `AGENTS.md` §17.2 /
//! §2.2 the difference is **discovered device-tree data**, never a
//! `cfg(board = …)` fork: this module owns the one console abstraction —
//! the `ConsoleModel` (a register layout) plus the runtime MMIO base —
//! while the freestanding `crate::serial` module performs the actual
//! register accesses against whatever this module currently holds.
//!
//! # Why two backends, not duplication
//!
//! The PL011 and the mini-UART have different transmit registers and
//! different "can I write a byte" status bits, so each is a distinct
//! `ConsoleModel` variant with its own pure register-offset / readiness
//! helpers. That is a driver-model split (one console, two register
//! backends), not the duplication `AGENTS.md` §2.2 forbids.
//!
//! # Pre-discovery default
//!
//! Until a boot path calls `configure` (or `configure_from_fdt`) the
//! console points at the QEMU `virt` PL011 base, so the early boot log and
//! the panic bridge print on `virt` with no discovery step. A board whose
//! console lives elsewhere (the Pi) overrides both base and model from its
//! firmware device tree. The default is the `virt` value, not a fabricated
//! per-board constant (`plans/PI.md` §3 — no fresh `PI_*_BASE`).

use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use rustos_fdt::Fdt;

/// MMIO base the console points at before any discovery runs: the QEMU
/// `virt` board's PrimeCell PL011 UART (fixed by the `virt` memory map,
/// wired to `-serial stdio`). A board with a different console replaces
/// this at boot from its device tree.
pub const DEFAULT_CONSOLE_BASE: usize = 0x0900_0000;

/// Register layout of a console UART. Both variants share the
/// `crate::serial` byte-at-a-time transmit path; they differ only in the
/// register offsets and the transmit-ready status bit, captured by the
/// pure helpers below so the freestanding MMIO code and the host unit
/// tests agree on one definition (`AGENTS.md` §2.2).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ConsoleModel {
    /// ARM PrimeCell PL011 (ARM DDI 0183): data register `UARTDR` at
    /// offset `0x00`, flag register `UARTFR` at `0x18` whose `TXFF`
    /// bit (5) marks the transmit FIFO full. Used by the QEMU `virt`
    /// board and the Pi's `UART0`.
    Pl011,
    /// BCM2835 AUX mini-UART (BCM2835 ARM Peripherals §2.2). The
    /// device-tree `reg` points at the mini-UART register block whose
    /// `AUX_MU_IO_REG` (data) sits at offset `0x00` and `AUX_MU_LSR_REG`
    /// (line status) at `0x14`; `LSR` bit 5 marks the transmitter able to
    /// accept a byte.
    MiniUart,
}

/// PL011 data register (`UARTDR`) offset.
const PL011_DR: usize = 0x00;
/// PL011 flag register (`UARTFR`) offset.
const PL011_FR: usize = 0x18;
/// `UARTFR.TXFF` — transmit FIFO full (bit 5). Clear means a byte fits.
const PL011_FR_TXFF: u32 = 1 << 5;

/// Mini-UART data register (`AUX_MU_IO_REG`) offset, relative to the
/// device-tree `reg` base.
const MINIUART_IO: usize = 0x00;
/// Mini-UART line-status register (`AUX_MU_LSR_REG`) offset, relative to
/// the device-tree `reg` base.
const MINIUART_LSR: usize = 0x14;
/// `AUX_MU_LSR_REG` bit 5 — "transmitter empty"/"can accept a byte". Set
/// means a byte fits.
const MINIUART_LSR_TX_EMPTY: u32 = 1 << 5;

impl ConsoleModel {
    /// The device-tree `compatible` string that selects each model.
    /// `serial0`/`UART0` is `arm,pl011`; the mini-UART is
    /// `brcm,bcm2835-aux-uart`.
    ///
    /// Returns `None` for any other string so an unknown console fails
    /// closed rather than guessing a register layout (`AGENTS.md` §2.9).
    #[must_use]
    pub fn from_compatible(compatible: &[u8]) -> Option<Self> {
        match compatible {
            b"arm,pl011" => Some(Self::Pl011),
            b"brcm,bcm2835-aux-uart" => Some(Self::MiniUart),
            _ => None,
        }
    }

    /// The canonical device-tree `compatible` string for this model — the
    /// inverse of [`Self::from_compatible`]. Used to label the discovered
    /// `serial` hardware-tree node's match key (`crate::platform`).
    #[must_use]
    pub const fn compatible(self) -> &'static [u8] {
        match self {
            Self::Pl011 => b"arm,pl011",
            Self::MiniUart => b"brcm,bcm2835-aux-uart",
        }
    }

    /// Raw discriminant stored in the [`AtomicU8`] console-model slot.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Pl011 => 0,
            Self::MiniUart => 1,
        }
    }

    /// Inverse of [`Self::as_u8`]. Any other value decodes to the
    /// fail-safe [`Self::Pl011`] default (the slot is only ever written by
    /// [`configure`], so an out-of-range value cannot occur in practice).
    #[must_use]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::MiniUart,
            _ => Self::Pl011,
        }
    }

    /// Byte offset of the data (transmit) register relative to the MMIO
    /// base.
    #[must_use]
    pub const fn data_offset(self) -> usize {
        match self {
            Self::Pl011 => PL011_DR,
            Self::MiniUart => MINIUART_IO,
        }
    }

    /// Byte offset of the transmit-status register relative to the MMIO
    /// base (PL011 `UARTFR` / mini-UART `AUX_MU_LSR_REG`).
    #[must_use]
    pub const fn status_offset(self) -> usize {
        match self {
            Self::Pl011 => PL011_FR,
            Self::MiniUart => MINIUART_LSR,
        }
    }

    /// Decode a status-register value into "the transmitter can accept a
    /// byte now". The two models use opposite-sense bits: the PL011's
    /// `TXFF` is *set* when full, the mini-UART's `LSR` bit 5 is *set* when
    /// it can accept — so the predicate is per-model, not shared.
    #[must_use]
    pub const fn tx_ready(self, status: u32) -> bool {
        match self {
            Self::Pl011 => status & PL011_FR_TXFF == 0,
            Self::MiniUart => status & MINIUART_LSR_TX_EMPTY != 0,
        }
    }
}

/// Currently-selected console MMIO base. Defaults to the `virt` PL011.
static CONSOLE_BASE: AtomicUsize = AtomicUsize::new(DEFAULT_CONSOLE_BASE);
/// Currently-selected console model discriminant. Defaults to
/// [`ConsoleModel::Pl011`].
static CONSOLE_MODEL: AtomicU8 = AtomicU8::new(ConsoleModel::Pl011.as_u8());

/// Point the console at `base` with register layout `model`.
///
/// Called once early in a board's boot path (after device-tree discovery
/// resolves the console). `Release`/`Acquire` ordering pairs the two
/// stores with [`current`]'s loads so the freestanding transmit path
/// observes a consistent `(base, model)` pair.
pub fn configure(base: usize, model: ConsoleModel) {
    // Publish the model first, then the base: the transmit path reads the
    // base last (Acquire), so once it sees the new base the matching model
    // is already visible.
    CONSOLE_MODEL.store(model.as_u8(), Ordering::Release);
    CONSOLE_BASE.store(base, Ordering::Release);
}

/// The console MMIO base and register layout currently in effect.
#[must_use]
pub fn current() -> (usize, ConsoleModel) {
    let base = CONSOLE_BASE.load(Ordering::Acquire);
    let model = ConsoleModel::from_u8(CONSOLE_MODEL.load(Ordering::Acquire));
    (base, model)
}

/// A console UART located in a flattened device tree.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct DiscoveredConsole {
    /// MMIO base of the UART register block (the node's first `reg` cell).
    pub base: u64,
    /// Length in bytes of the register window (the node's first `reg`
    /// size cell).
    pub len: u64,
    /// The register layout selected by the node's `compatible` string.
    pub model: ConsoleModel,
}

/// Locate the console UART in `fdt`.
///
/// Walks the tree for the first node whose `compatible` names a model
/// this port speaks ([`ConsoleModel::from_compatible`]), preferring a
/// PL011 over a mini-UART when a board exposes both (the Pi wires its
/// primary console — `serial0`/`ttyAMA0` — to the PL011, and that is what
/// QEMU's `raspi*` models route to `-serial`). Returns `None` if the tree
/// is malformed or carries no recognised console (the caller then keeps
/// the [`DEFAULT_CONSOLE_BASE`] default — fail closed, `AGENTS.md` §2.9).
#[must_use]
pub fn find_console(fdt: &Fdt<'_>) -> Option<DiscoveredConsole> {
    let mut mini_uart: Option<DiscoveredConsole> = None;
    for node in fdt.nodes() {
        // A malformed token ends enumeration; a console found before it
        // still counts, otherwise we fail closed.
        let Ok(node) = node else { break };
        let Some(compatible) = node.property("compatible") else {
            continue;
        };
        let Some(model) = compatible
            .iter_strings()
            .find_map(ConsoleModel::from_compatible)
        else {
            continue;
        };
        let Some(reg) = node.property("reg") else {
            continue;
        };
        let (Ok(base), Ok(len)) = (reg.read_be_u64(0), reg.read_be_u64(8)) else {
            continue;
        };
        let found = DiscoveredConsole { base, len, model };
        match model {
            // A PL011 is the preferred console; take it immediately.
            ConsoleModel::Pl011 => return Some(found),
            // Remember the first mini-UART but keep scanning for a PL011.
            ConsoleModel::MiniUart => {
                if mini_uart.is_none() {
                    mini_uart = Some(found);
                }
            }
        }
    }
    mini_uart
}

/// Discover the console in `fdt` and point the console at it.
///
/// Returns the [`DiscoveredConsole`] that was applied, or `None` (leaving
/// the previous configuration untouched) when the tree carries no
/// recognised console or its base does not fit a `usize`.
#[must_use]
pub fn configure_from_fdt(fdt: &Fdt<'_>) -> Option<DiscoveredConsole> {
    let found = find_console(fdt)?;
    // A device MMIO base always fits a `usize` on the 64-bit targets this
    // port serves; `try_from` keeps the conversion honest — fail closed
    // rather than truncate — instead of asserting it (`AGENTS.md` §2.9).
    let base = usize::try_from(found.base).ok()?;
    configure(base, found.model);
    Some(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_fdt::fixture::{raspi_like_arm, virt_like_arm};

    #[test]
    fn compatible_selects_the_register_layout() {
        assert_eq!(
            ConsoleModel::from_compatible(b"arm,pl011"),
            Some(ConsoleModel::Pl011)
        );
        assert_eq!(
            ConsoleModel::from_compatible(b"brcm,bcm2835-aux-uart"),
            Some(ConsoleModel::MiniUart)
        );
        assert_eq!(ConsoleModel::from_compatible(b"ns16550a"), None);
        assert_eq!(ConsoleModel::from_compatible(b""), None);
    }

    #[test]
    fn compatible_string_round_trips_through_the_model() {
        for model in [ConsoleModel::Pl011, ConsoleModel::MiniUart] {
            assert_eq!(
                ConsoleModel::from_compatible(model.compatible()),
                Some(model)
            );
        }
    }

    #[test]
    fn model_discriminant_round_trips() {
        for model in [ConsoleModel::Pl011, ConsoleModel::MiniUart] {
            assert_eq!(ConsoleModel::from_u8(model.as_u8()), model);
        }
        // Any out-of-range discriminant decodes to the safe PL011 default.
        assert_eq!(ConsoleModel::from_u8(2), ConsoleModel::Pl011);
        assert_eq!(ConsoleModel::from_u8(255), ConsoleModel::Pl011);
    }

    #[test]
    fn pl011_register_offsets_and_readiness() {
        let m = ConsoleModel::Pl011;
        assert_eq!(m.data_offset(), 0x00);
        assert_eq!(m.status_offset(), 0x18);
        // TXFF (bit 5) set => FIFO full => not ready.
        assert!(!m.tx_ready(0x20));
        assert!(m.tx_ready(0x00));
        // Other flag bits do not affect readiness.
        assert!(m.tx_ready(0x01));
    }

    #[test]
    fn miniuart_register_offsets_and_readiness() {
        let m = ConsoleModel::MiniUart;
        assert_eq!(m.data_offset(), 0x00);
        assert_eq!(m.status_offset(), 0x14);
        // LSR bit 5 set => transmitter can accept a byte => ready.
        assert!(m.tx_ready(0x20));
        assert!(!m.tx_ready(0x00));
        // Other LSR bits (e.g. data-ready bit 0) do not mark TX ready.
        assert!(!m.tx_ready(0x01));
    }

    #[test]
    fn finds_pl011_console_in_a_raspi_tree() {
        // The Pi tree carries both a PL011 and a mini-UART; the PL011 is
        // the preferred console.
        let blob = raspi_like_arm(0x3f20_1000, 0x3f21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let console = find_console(&fdt).expect("a console is present");
        assert_eq!(console.model, ConsoleModel::Pl011);
        assert_eq!(console.base, 0x3f20_1000);
        assert_eq!(console.len, 0x1000);
    }

    #[test]
    fn falls_back_to_mini_uart_when_no_pl011() {
        // A tree with only the AUX mini-UART selects it.
        let blob = raspi_like_arm(0, 0x3f21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let console = find_console(&fdt).expect("mini-uart present");
        assert_eq!(console.model, ConsoleModel::MiniUart);
        assert_eq!(console.base, 0x3f21_5040);
        assert_eq!(console.len, 0x40);
    }

    #[test]
    fn no_console_in_a_uartless_tree_is_none() {
        // The `virt`-shaped fixture carries no UART node.
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 14);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(find_console(&fdt), None);
    }

    #[test]
    fn configure_from_fdt_applies_the_discovered_base() {
        // Drive the global config through an FDT and read it back. This
        // test owns the global console state for its duration; the other
        // tests in this module do not call `configure`, so there is no
        // cross-test interference (they only exercise pure helpers).
        let blob = raspi_like_arm(0x3f20_1000, 0x3f21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let applied = configure_from_fdt(&fdt).expect("console discovered");
        assert_eq!(applied.model, ConsoleModel::Pl011);
        let (base, model) = current();
        assert_eq!(base, 0x3f20_1000);
        assert_eq!(model, ConsoleModel::Pl011);

        // Restore the default so the process-global slot is left as other
        // host tests expect (defence-in-depth; nothing else reads it).
        configure(DEFAULT_CONSOLE_BASE, ConsoleModel::Pl011);
    }
}
