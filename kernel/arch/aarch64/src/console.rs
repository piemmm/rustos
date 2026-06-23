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
/// `UARTFR.RXFE` — receive FIFO empty (bit 4). Clear means a byte is
/// available to read.
const PL011_FR_RXFE: u32 = 1 << 4;

/// PL011 interrupt mask set/clear register (`UARTIMSC`) offset. Writing a
/// bit set unmasks (enables) that interrupt source.
const PL011_IMSC: usize = 0x38;
/// `UARTIMSC.RXIM` — receive interrupt (bit 4): fires when the receive FIFO
/// crosses its trigger level.
const PL011_IMSC_RXIM: u32 = 1 << 4;
/// `UARTIMSC.RTIM` — receive-timeout interrupt (bit 6): fires when received
/// bytes sit below the trigger level for the timeout, so a single typed
/// byte still raises an interrupt regardless of the FIFO trigger.
const PL011_IMSC_RTIM: u32 = 1 << 6;
/// PL011 control register (`UARTCR`) offset.
const PL011_CR: usize = 0x30;
/// `UARTCR.UARTEN` (bit 0) — UART enable.
const PL011_CR_UARTEN: u32 = 1 << 0;
/// `UARTCR.RXE` (bit 9) — receiver enable (a received byte is only latched,
/// and thus only raises a receive interrupt, while this is set).
const PL011_CR_RXE: u32 = 1 << 9;
/// PL011 interrupt-clear register (`UARTICR`) offset. Write-1-to-clear: a
/// set bit clears the corresponding latched interrupt.
const PL011_ICR: usize = 0x44;
/// `UARTICR.RXIC` (bit 4) and `UARTICR.RTIC` (bit 6) — clear the receive and
/// receive-timeout interrupt latches. The receive-timeout latch in
/// particular is *not* cleared merely by emptying the FIFO, so an ISR that
/// drained an empty FIFO must write these or the line re-asserts forever.
const PL011_ICR_RXIC_RTIC: u32 = (1 << 4) | (1 << 6);
/// PL011 interrupt FIFO level-select register (`UARTIFLS`) offset.
const PL011_IFLS: usize = 0x34;
/// `UARTIFLS.RXIFLSEL` — receive FIFO trigger select (bits 5:3). Clearing it
/// selects the lowest (1/8-full) trigger so the receive interrupt fires as
/// promptly as the FIFO allows.
const PL011_IFLS_RXSEL: u32 = 0b111 << 3;
/// `UARTIMSC.TXIM` — transmit interrupt (bit 5): asserts while the transmit
/// FIFO is at or below its trigger level (i.e. has room), so unmasking it
/// drives an ISR that refills the FIFO from the software ring until the ring
/// drains, then re-masks it (`crate::serial`).
const PL011_IMSC_TXIM: u32 = 1 << 5;
/// PL011 masked interrupt-status register (`UARTMIS`) offset: the raw
/// interrupt state masked by `UARTIMSC`, so a bit is set only for a source
/// that is both pending *and* unmasked. The ISR reads it to act on exactly
/// the sources that fired (`AGENTS.md` §5.4 — never drain receive bytes the
/// poll path still owns: while `RXIM` is masked, `RXMIS` reads clear).
const PL011_MIS: usize = 0x40;
/// `UARTMIS.RXMIS` (bit 4) — a receive interrupt is pending and unmasked.
const PL011_MIS_RXMIS: u32 = 1 << 4;
/// `UARTMIS.TXMIS` (bit 5) — a transmit interrupt is pending and unmasked.
const PL011_MIS_TXMIS: u32 = 1 << 5;
/// `UARTMIS.RTMIS` (bit 6) — a receive-timeout interrupt is pending and
/// unmasked.
const PL011_MIS_RTMIS: u32 = 1 << 6;
/// `UARTICR.TXIC` (bit 5) — clear the latched transmit interrupt. Masking
/// `TXIM` already removes it from `UARTMIS`; this clears the underlying latch
/// so a later re-enable starts from a clean state.
const PL011_ICR_TXIC: u32 = 1 << 5;

/// Mini-UART interrupt-enable register (`AUX_MU_IER_REG`) offset, relative
/// to the device-tree `reg` base.
const MINIUART_IER: usize = 0x04;
/// `AUX_MU_IER_REG` bit 0 — enable the receive interrupt.
const MINIUART_IER_RX: u32 = 0x01;
/// `AUX_MU_IER_REG` bit 1 — enable the transmit interrupt (asserts while the
/// transmit holding register/FIFO can accept a byte).
const MINIUART_IER_TX: u32 = 0x02;
/// Mini-UART interrupt-identify register (`AUX_MU_IIR_REG`) offset, relative
/// to the device-tree `reg` base: the mini-UART's analogue of the PL011's
/// `UARTMIS` — it reports which enabled source is pending. Bit 0 is *clear*
/// while an interrupt is pending, and bits 2:1 identify it.
const MINIUART_IIR: usize = 0x08;
/// `AUX_MU_IIR_REG` bits 2:1 isolate the pending-interrupt identity.
const MINIUART_IIR_ID: u32 = 0b110;
/// `AUX_MU_IIR_REG` bits 2:1 == `0b01` — transmit holding register empty
/// (the transmit interrupt).
const MINIUART_IIR_TX: u32 = 0b010;
/// `AUX_MU_IIR_REG` bits 2:1 == `0b10` — receiver holds a valid byte (the
/// receive interrupt).
const MINIUART_IIR_RX: u32 = 0b100;

/// Mini-UART data register (`AUX_MU_IO_REG`) offset, relative to the
/// device-tree `reg` base.
const MINIUART_IO: usize = 0x00;
/// Mini-UART line-status register (`AUX_MU_LSR_REG`) offset, relative to
/// the device-tree `reg` base.
const MINIUART_LSR: usize = 0x14;
/// `AUX_MU_LSR_REG` bit 5 — "transmitter empty"/"can accept a byte". Set
/// means a byte fits.
const MINIUART_LSR_TX_EMPTY: u32 = 1 << 5;
/// `AUX_MU_LSR_REG` bit 0 — "data ready". Set means the receive FIFO holds
/// at least one byte to read.
const MINIUART_LSR_DATA_READY: u32 = 1 << 0;

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

    /// Decode a status-register value into "a received byte is available to
    /// read now". The receive-status register coincides with the
    /// transmit-status one ([`Self::status_offset`]) on both models —
    /// PL011 `UARTFR`, mini-UART `AUX_MU_LSR_REG` — but the bit differs:
    /// the PL011's `RXFE` (bit 4) is *set* when the FIFO is **empty**, the
    /// mini-UART's `LSR` bit 0 is *set* when data is **ready**, so the
    /// predicate is per-model, not shared. The read data register also
    /// coincides with the transmit one ([`Self::data_offset`]) — PL011
    /// `UARTDR`, mini-UART `AUX_MU_IO_REG` — so no extra offset accessor is
    /// needed for the receive path.
    #[must_use]
    pub const fn rx_ready(self, status: u32) -> bool {
        match self {
            Self::Pl011 => status & PL011_FR_RXFE == 0,
            Self::MiniUart => status & MINIUART_LSR_DATA_READY != 0,
        }
    }

    /// The read-modify-write steps that switch this UART from poll-only to
    /// **receive-interrupt-driven**, in order.
    ///
    /// The freestanding applier (`serial::enable_rx_interrupt`)
    /// performs each non-empty step as `*reg = (*reg & !clear) | set` against
    /// the model's documented registers; an empty step ([`RegRmw::is_noop`])
    /// is skipped, so the fixed-length array carries the longer PL011 sequence
    /// and pads the shorter mini-UART one. Keeping the register/bit policy here
    /// (beside the offset/readiness helpers) and the MMIO in `crate::serial`
    /// is the same pure-policy / freestanding-driver split the rest of this
    /// module uses (`AGENTS.md` §2.2), so the sequence is host-tested without
    /// touching MMIO.
    ///
    /// - **PL011:** select the lowest receive FIFO trigger (`UARTIFLS`), enable
    ///   the UART and receiver (`UARTCR`), then unmask the receive and
    ///   receive-timeout interrupts (`UARTIMSC`) — the timeout source ensures a
    ///   single typed byte interrupts even below the FIFO trigger.
    /// - **Mini-UART:** set the receive-interrupt-enable bit (`AUX_MU_IER_REG`).
    #[must_use]
    pub const fn rx_interrupt_sequence(self) -> [RegRmw; 3] {
        match self {
            Self::Pl011 => [
                RegRmw {
                    offset: PL011_IFLS,
                    clear: PL011_IFLS_RXSEL,
                    set: 0,
                },
                RegRmw {
                    offset: PL011_CR,
                    clear: 0,
                    set: PL011_CR_UARTEN | PL011_CR_RXE,
                },
                RegRmw {
                    offset: PL011_IMSC,
                    clear: 0,
                    set: PL011_IMSC_RXIM | PL011_IMSC_RTIM,
                },
            ],
            Self::MiniUart => [
                RegRmw {
                    offset: MINIUART_IER,
                    clear: 0,
                    set: MINIUART_IER_RX,
                },
                RegRmw::NOOP,
                RegRmw::NOOP,
            ],
        }
    }

    /// The write that clears this UART's latched receive / receive-timeout
    /// interrupt, as `(offset, value)` for the register at `offset` from the
    /// MMIO base, or [`None`] when the model has no write-1-to-clear register
    /// and the latch is cleared simply by emptying the FIFO.
    ///
    /// A receive ISR that drains the FIFO **empty** must apply this (when
    /// present): the PL011's receive-timeout latch is not cleared by reading
    /// data once the FIFO is empty, so without the `UARTICR` write the line
    /// stays asserted and the ISR re-fires forever (an interrupt storm). The
    /// mini-UART's receive interrupt clears when the data register is read, so
    /// it needs no separate clear ([`None`]).
    #[must_use]
    pub const fn rx_interrupt_clear(self) -> Option<(usize, u32)> {
        match self {
            Self::Pl011 => Some((PL011_ICR, PL011_ICR_RXIC_RTIC)),
            Self::MiniUart => None,
        }
    }

    /// The read-modify-write that **unmasks** this UART's transmit interrupt,
    /// applied by the freestanding `serial::enable_tx_interrupt`.
    ///
    /// The transmit interrupt asserts while the transmit FIFO has room (at or
    /// below its trigger level), so once it is unmasked the ISR refills the
    /// FIFO from the software ring until the ring drains and then re-masks it
    /// ([`Self::tx_interrupt_disable`]). This is the interrupt-driven transmit
    /// path that keeps buffered output flowing at the UART's real throughput
    /// without coupling the drain to the scheduler reaching idle (`AGENTS.md`
    /// §2.16 / §20). It touches only the interrupt-enable register
    /// (PL011 `UARTIMSC.TXIM`, mini-UART `AUX_MU_IER_REG` bit 1) and never the
    /// receive bits, so unmasking transmit leaves the receive line exactly as
    /// the boot/login path set it.
    #[must_use]
    pub const fn tx_interrupt_enable(self) -> RegRmw {
        match self {
            Self::Pl011 => RegRmw {
                offset: PL011_IMSC,
                clear: 0,
                set: PL011_IMSC_TXIM,
            },
            Self::MiniUart => RegRmw {
                offset: MINIUART_IER,
                clear: 0,
                set: MINIUART_IER_TX,
            },
        }
    }

    /// The read-modify-write that **masks** this UART's transmit interrupt —
    /// the inverse of [`Self::tx_interrupt_enable`], applied by
    /// `serial::disable_tx_interrupt` once the ring drains so an empty FIFO
    /// does not re-fire the ISR forever.
    #[must_use]
    pub const fn tx_interrupt_disable(self) -> RegRmw {
        match self {
            Self::Pl011 => RegRmw {
                offset: PL011_IMSC,
                clear: PL011_IMSC_TXIM,
                set: 0,
            },
            Self::MiniUart => RegRmw {
                offset: MINIUART_IER,
                clear: MINIUART_IER_TX,
                set: 0,
            },
        }
    }

    /// Byte offset of the register the ISR reads to learn which interrupt
    /// sources are pending **and unmasked**: PL011 `UARTMIS`, mini-UART
    /// `AUX_MU_IIR_REG`.
    ///
    /// Reading exactly the masked status is what lets one shared interrupt
    /// line carry both transmit and receive without the ISR ever draining
    /// receive bytes the poll path still owns: while the receive source is
    /// masked it never appears here, so the passphrase FIFO-poll keeps its
    /// bytes (`AGENTS.md` §5.4 — fail closed by construction).
    #[must_use]
    pub const fn interrupt_status_offset(self) -> usize {
        match self {
            Self::Pl011 => PL011_MIS,
            Self::MiniUart => MINIUART_IIR,
        }
    }

    /// Decode an [`Self::interrupt_status_offset`] read into "the transmit
    /// interrupt fired". PL011: `UARTMIS.TXMIS`. Mini-UART: the
    /// pending-identity field (`AUX_MU_IIR_REG` bits 2:1) equals the
    /// transmit code.
    #[must_use]
    pub const fn tx_interrupt_fired(self, status: u32) -> bool {
        match self {
            Self::Pl011 => status & PL011_MIS_TXMIS != 0,
            Self::MiniUart => status & MINIUART_IIR_ID == MINIUART_IIR_TX,
        }
    }

    /// Decode an [`Self::interrupt_status_offset`] read into "a receive
    /// interrupt fired" (data available or receive-timeout). PL011:
    /// `UARTMIS.RXMIS | RTMIS`. Mini-UART: the pending-identity field equals
    /// the receive code. Reads clear while the receive source is masked, so
    /// this is `false` throughout the passphrase poll window.
    #[must_use]
    pub const fn rx_interrupt_fired(self, status: u32) -> bool {
        match self {
            Self::Pl011 => status & (PL011_MIS_RXMIS | PL011_MIS_RTMIS) != 0,
            Self::MiniUart => status & MINIUART_IIR_ID == MINIUART_IIR_RX,
        }
    }

    /// The write that clears this UART's latched **transmit** interrupt, as
    /// `(offset, value)`, or [`None`] when the model needs none.
    ///
    /// Masking [`Self::tx_interrupt_disable`] already removes the source from
    /// the masked status, so this is belt-and-braces for the PL011 (clear the
    /// `UARTICR.TXIC` latch so a later re-enable starts clean). The mini-UART
    /// clears its transmit interrupt when the identify register is read, so it
    /// needs no separate write ([`None`]).
    #[must_use]
    pub const fn tx_interrupt_clear(self) -> Option<(usize, u32)> {
        match self {
            Self::Pl011 => Some((PL011_ICR, PL011_ICR_TXIC)),
            Self::MiniUart => None,
        }
    }
}

/// One read-modify-write applied to a console register:
/// `*reg = (*reg & !clear) | set`, where `reg` is the register at byte
/// `offset` from the console MMIO base.
///
/// Produced by the model's interrupt-control accessors
/// ([`ConsoleModel::rx_interrupt_sequence`], [`ConsoleModel::tx_interrupt_enable`])
/// and applied by the freestanding `serial` MMIO helpers. A [`RegRmw::NOOP`]
/// (both masks zero) is a padding entry the applier skips.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct RegRmw {
    /// Byte offset of the target register from the console MMIO base.
    pub offset: usize,
    /// Bits to clear before OR-ing in [`Self::set`].
    pub clear: u32,
    /// Bits to set.
    pub set: u32,
}

impl RegRmw {
    /// A do-nothing step: both masks zero, so the applier skips it. Pads the
    /// fixed-length sequence for models that need fewer than three writes.
    pub const NOOP: Self = Self {
        offset: 0,
        clear: 0,
        set: 0,
    };

    /// Whether this step changes nothing (both masks zero) and is skipped by
    /// the applier.
    #[must_use]
    pub const fn is_noop(self) -> bool {
        self.clear == 0 && self.set == 0
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
    /// CPU-physical MMIO base of the UART register block: the node's
    /// first `reg` entry, decoded with its parent bus's cell counts and
    /// translated through the ancestor buses' `ranges`
    /// ([`crate::fdt::translated_reg`]).
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
/// QEMU's `raspi*` models route to `-serial`). The node's `reg` is
/// decoded with its parent bus's cell counts and translated through the
/// ancestor buses' `ranges` ([`crate::fdt::translated_reg`]) — on the
/// real Pi 4 tree the UARTs sit under `/soc`, whose one-cell `reg`
/// values are *bus* addresses (`0x7E20_1000`) remapped to CPU-physical
/// space (`0xFE20_1000`). The walk early-returns at the matched PL011,
/// so it stays safe with the MMU off ([`crate::fdt::scan_translated`]).
/// Returns `None` if the tree is malformed or carries no recognised,
/// translatable console (the caller then keeps the
/// [`DEFAULT_CONSOLE_BASE`] default — fail closed, `AGENTS.md` §2.9).
#[must_use]
pub fn find_console(fdt: &Fdt<'_>) -> Option<DiscoveredConsole> {
    let mut mini_uart: Option<DiscoveredConsole> = None;
    let pl011 = crate::fdt::scan_translated(fdt, |node, levels, depth| {
        let model = node
            .property("compatible")?
            .iter_strings()
            .find_map(ConsoleModel::from_compatible)?;
        let (base, len) = crate::fdt::translated_reg(node, depth, levels, 0)?;
        let found = DiscoveredConsole { base, len, model };
        match model {
            // A PL011 is the preferred console; take it immediately.
            ConsoleModel::Pl011 => Some(found),
            // Remember the first mini-UART but keep scanning for a PL011.
            ConsoleModel::MiniUart => {
                if mini_uart.is_none() {
                    mini_uart = Some(found);
                }
                None
            }
        }
    });
    pl011.or(mini_uart)
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

/// Most transmit-ready polls one byte may consume before the UART is
/// declared wedged and the byte dropped ([`tx_wait`]).
///
/// Sized for the slowest healthy drain the console supports: a full
/// 16-deep PL011 FIFO at 9600 baud empties in ≈ 17 ms, and an MMIO
/// status poll on the BCM2711 costs well over 100 ns, so the budget
/// covers that drain with generous headroom. A transmitter that is
/// still not ready after the budget is not draining at all — on the Pi
/// 4 this is the BT-attached PL011 whose CTS flow control never opens —
/// and waiting longer would hang the boot (`AGENTS.md` §2.1: an
/// unbounded wait stalls the kernel before its first log line).
pub const TX_POLL_BUDGET: u32 = 200_000;

/// Verdict of one bounded transmit-readiness wait ([`tx_wait`]).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TxOutcome {
    /// The transmitter can accept the byte: write it.
    Send,
    /// The transmitter never became ready: drop the byte (the console
    /// is best-effort output and must never stall the kernel —
    /// `AGENTS.md` §2.1 / §20 fail-closed no-op semantics).
    Drop,
}

/// Wait — boundedly — for the transmitter to accept a byte.
///
/// `tx_ready` polls the device's readiness bit; `wedged` is the sticky
/// verdict of the previous wait. A non-wedged transmitter is polled up
/// to `budget` times; expiry declares it wedged and drops the byte. A
/// wedged transmitter is polled exactly once per byte — recovering the
/// moment the FIFO drains, dropping the byte otherwise — so a UART that
/// never drains (the Pi 4's BT-attached, flow-blocked PL011) costs the
/// budget once, not per byte. Returns the verdict and the new wedged
/// state.
///
/// Pure over the `tx_ready` closure so the policy is host-tested; the
/// freestanding `crate::serial` transmit path supplies the MMIO poll on
/// the target.
pub fn tx_wait(mut tx_ready: impl FnMut() -> bool, wedged: bool, budget: u32) -> (TxOutcome, bool) {
    if wedged {
        return if tx_ready() {
            (TxOutcome::Send, false)
        } else {
            (TxOutcome::Drop, true)
        };
    }
    let mut remaining = budget;
    while remaining != 0 {
        if tx_ready() {
            return (TxOutcome::Send, false);
        }
        remaining -= 1;
        core::hint::spin_loop();
    }
    (TxOutcome::Drop, true)
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
    fn pl011_rx_readiness() {
        let m = ConsoleModel::Pl011;
        // RXFE (bit 4) set => receive FIFO empty => no byte to read.
        assert!(!m.rx_ready(0x10));
        // RXFE clear => a received byte is available.
        assert!(m.rx_ready(0x00));
        // Other flag bits (e.g. TXFF bit 5) do not mark RX ready.
        assert!(!m.rx_ready(0x30));
        assert!(m.rx_ready(0x20));
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
    fn miniuart_rx_readiness() {
        let m = ConsoleModel::MiniUart;
        // LSR bit 0 set => data ready => a byte is available.
        assert!(m.rx_ready(0x01));
        assert!(!m.rx_ready(0x00));
        // Other LSR bits (e.g. TX-empty bit 5) do not mark RX ready.
        assert!(!m.rx_ready(0x20));
        assert!(m.rx_ready(0x21));
    }

    #[test]
    fn pl011_rx_interrupt_sequence_enables_rx_and_timeout() {
        let seq = ConsoleModel::Pl011.rx_interrupt_sequence();
        // Three live steps, none padding: IFLS trigger, CR enable, IMSC unmask.
        assert!(seq.iter().all(|s| !s.is_noop()));
        // IFLS: clear the receive trigger-select field (select 1/8), set nothing.
        assert_eq!(
            seq[0],
            RegRmw {
                offset: 0x34,
                clear: 0b111 << 3,
                set: 0
            }
        );
        // CR: enable the UART (bit 0) and the receiver (bit 9).
        assert_eq!(
            seq[1],
            RegRmw {
                offset: 0x30,
                clear: 0,
                set: (1 << 0) | (1 << 9)
            }
        );
        // IMSC: unmask the receive (bit 4) and receive-timeout (bit 6) sources.
        assert_eq!(
            seq[2],
            RegRmw {
                offset: 0x38,
                clear: 0,
                set: (1 << 4) | (1 << 6)
            }
        );
    }

    #[test]
    fn miniuart_rx_interrupt_sequence_sets_the_ier_bit_and_pads() {
        let seq = ConsoleModel::MiniUart.rx_interrupt_sequence();
        // One live step (IER RX-enable) followed by two skipped padding steps.
        assert_eq!(
            seq[0],
            RegRmw {
                offset: 0x04,
                clear: 0,
                set: 0x01
            }
        );
        assert!(seq[1].is_noop());
        assert!(seq[2].is_noop());
    }

    #[test]
    fn rx_interrupt_clear_targets_icr_for_pl011_and_none_for_miniuart() {
        // PL011: write RXIC|RTIC (bits 4 and 6) to UARTICR (0x44) to clear the
        // latched receive / receive-timeout interrupt.
        assert_eq!(
            ConsoleModel::Pl011.rx_interrupt_clear(),
            Some((0x44, (1 << 4) | (1 << 6)))
        );
        // Mini-UART: the receive interrupt clears on a data-register read, so
        // no separate clear write is needed.
        assert_eq!(ConsoleModel::MiniUart.rx_interrupt_clear(), None);
    }

    #[test]
    fn tx_interrupt_enable_unmasks_only_the_tx_source() {
        // PL011: set UARTIMSC.TXIM (bit 5), touching no receive bit, so the
        // passphrase FIFO-poll's masked receive line is undisturbed.
        assert_eq!(
            ConsoleModel::Pl011.tx_interrupt_enable(),
            RegRmw {
                offset: 0x38,
                clear: 0,
                set: 1 << 5
            }
        );
        // Mini-UART: set AUX_MU_IER_REG bit 1 (TX), again leaving the RX bit.
        assert_eq!(
            ConsoleModel::MiniUart.tx_interrupt_enable(),
            RegRmw {
                offset: 0x04,
                clear: 0,
                set: 0x02
            }
        );
    }

    #[test]
    fn tx_interrupt_disable_is_the_inverse_of_enable() {
        // PL011: clear UARTIMSC.TXIM.
        assert_eq!(
            ConsoleModel::Pl011.tx_interrupt_disable(),
            RegRmw {
                offset: 0x38,
                clear: 1 << 5,
                set: 0
            }
        );
        // Mini-UART: clear AUX_MU_IER_REG bit 1.
        assert_eq!(
            ConsoleModel::MiniUart.tx_interrupt_disable(),
            RegRmw {
                offset: 0x04,
                clear: 0x02,
                set: 0
            }
        );
    }

    #[test]
    fn interrupt_status_decode_separates_tx_and_rx_per_model() {
        // PL011 reads UARTMIS (0x40); TXMIS=bit5, RXMIS=bit4, RTMIS=bit6.
        assert_eq!(ConsoleModel::Pl011.interrupt_status_offset(), 0x40);
        assert!(ConsoleModel::Pl011.tx_interrupt_fired(1 << 5));
        assert!(!ConsoleModel::Pl011.rx_interrupt_fired(1 << 5));
        assert!(ConsoleModel::Pl011.rx_interrupt_fired(1 << 4));
        assert!(ConsoleModel::Pl011.rx_interrupt_fired(1 << 6));
        assert!(!ConsoleModel::Pl011.tx_interrupt_fired(1 << 4));
        // A masked source never appears in UARTMIS, so a zero read fires
        // neither — the property the passphrase poll relies on.
        assert!(!ConsoleModel::Pl011.tx_interrupt_fired(0));
        assert!(!ConsoleModel::Pl011.rx_interrupt_fired(0));

        // Mini-UART reads AUX_MU_IIR_REG (0x08); the identity is bits 2:1.
        assert_eq!(ConsoleModel::MiniUart.interrupt_status_offset(), 0x08);
        assert!(ConsoleModel::MiniUart.tx_interrupt_fired(0b010));
        assert!(!ConsoleModel::MiniUart.rx_interrupt_fired(0b010));
        assert!(ConsoleModel::MiniUart.rx_interrupt_fired(0b100));
        assert!(!ConsoleModel::MiniUart.tx_interrupt_fired(0b100));
        // "No interrupt pending" (identity 0b00) fires neither.
        assert!(!ConsoleModel::MiniUart.tx_interrupt_fired(0b001));
        assert!(!ConsoleModel::MiniUart.rx_interrupt_fired(0b001));
    }

    #[test]
    fn tx_interrupt_clear_targets_icr_for_pl011_and_none_for_miniuart() {
        // PL011: write TXIC (bit 5) to UARTICR (0x44) to clear the latch.
        assert_eq!(
            ConsoleModel::Pl011.tx_interrupt_clear(),
            Some((0x44, 1 << 5))
        );
        // Mini-UART: the transmit interrupt clears on an identify-register
        // read, so no separate clear write is needed.
        assert_eq!(ConsoleModel::MiniUart.tx_interrupt_clear(), None);
    }

    #[test]
    fn rx_int_rmw_noop_is_recognised() {
        assert!(RegRmw::NOOP.is_noop());
        assert!(!RegRmw {
            offset: 0,
            clear: 0,
            set: 1
        }
        .is_noop());
        // A clear-only step still changes the register, so it is not a no-op.
        assert!(!RegRmw {
            offset: 0,
            clear: 1,
            set: 0
        }
        .is_noop());
    }

    #[test]
    fn finds_pl011_console_in_a_raspi_tree() {
        // The Pi tree carries both a PL011 and a mini-UART under `/soc`;
        // the PL011 is the preferred console, and its one-cell bus `reg`
        // (`0x7E20_1000`) is translated through the `/soc` `ranges` to
        // the CPU-physical base the real BCM2711 maps it at.
        let blob = raspi_like_arm(0x7e20_1000, 0x7e21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let console = find_console(&fdt).expect("a console is present");
        assert_eq!(console.model, ConsoleModel::Pl011);
        assert_eq!(console.base, 0xfe20_1000);
        assert_eq!(console.len, 0x200);
    }

    #[test]
    fn falls_back_to_mini_uart_when_no_pl011() {
        // A tree with only the AUX mini-UART selects it, translated
        // through the `/soc` `ranges` like every other window.
        let blob = raspi_like_arm(0, 0x7e21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let console = find_console(&fdt).expect("mini-uart present");
        assert_eq!(console.model, ConsoleModel::MiniUart);
        assert_eq!(console.base, 0xfe21_5040);
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
        let blob = raspi_like_arm(0x7e20_1000, 0x7e21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let applied = configure_from_fdt(&fdt).expect("console discovered");
        assert_eq!(applied.model, ConsoleModel::Pl011);
        let (base, model) = current();
        assert_eq!(base, 0xfe20_1000);
        assert_eq!(model, ConsoleModel::Pl011);

        // Restore the default so the process-global slot is left as other
        // host tests expect (defence-in-depth; nothing else reads it).
        configure(DEFAULT_CONSOLE_BASE, ConsoleModel::Pl011);
    }

    #[test]
    fn tx_wait_sends_immediately_when_ready() {
        let mut polls = 0u32;
        let (outcome, wedged) = tx_wait(
            || {
                polls += 1;
                true
            },
            false,
            TX_POLL_BUDGET,
        );
        assert_eq!(outcome, TxOutcome::Send);
        assert!(!wedged);
        assert_eq!(polls, 1);
    }

    #[test]
    fn tx_wait_sends_after_a_slow_drain_within_budget() {
        let mut polls = 0u32;
        let (outcome, wedged) = tx_wait(
            || {
                polls += 1;
                polls == 7
            },
            false,
            16,
        );
        assert_eq!(outcome, TxOutcome::Send);
        assert!(!wedged);
        assert_eq!(polls, 7);
    }

    #[test]
    fn tx_wait_declares_a_never_ready_transmitter_wedged() {
        let mut polls = 0u32;
        let (outcome, wedged) = tx_wait(
            || {
                polls += 1;
                false
            },
            false,
            16,
        );
        assert_eq!(outcome, TxOutcome::Drop);
        assert!(wedged);
        assert_eq!(polls, 16);
    }

    #[test]
    fn tx_wait_polls_a_wedged_transmitter_once_and_drops() {
        let mut polls = 0u32;
        let (outcome, wedged) = tx_wait(
            || {
                polls += 1;
                false
            },
            true,
            TX_POLL_BUDGET,
        );
        assert_eq!(outcome, TxOutcome::Drop);
        assert!(wedged);
        assert_eq!(polls, 1);
    }

    #[test]
    fn tx_wait_recovers_a_wedged_transmitter_when_the_fifo_drains() {
        let mut polls = 0u32;
        let (outcome, wedged) = tx_wait(
            || {
                polls += 1;
                true
            },
            true,
            TX_POLL_BUDGET,
        );
        assert_eq!(outcome, TxOutcome::Send);
        assert!(!wedged);
        assert_eq!(polls, 1);
    }

    #[test]
    fn tx_wait_with_zero_budget_drops_and_wedges() {
        let (outcome, wedged) = tx_wait(|| true, false, 0);
        assert_eq!(outcome, TxOutcome::Drop);
        assert!(wedged);
    }
}
