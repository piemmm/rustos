//! BCM2711 EMMC2 (Arasan / SDHCI-5.1) register map and bit fields.
//!
//! Byte offsets and bit positions follow the SD Host Controller Simplified
//! Specification (v3.00) standard register block, which the Pi 4 EMMC2
//! controller implements. Only the registers the driver drives are
//! named; an unused register is not declared.

/// SDHCI standard register block length, in bytes. The Pi 4 device tree
/// advertises a `0x100`-byte window for the `brcm,bcm2711-emmc2` node;
/// the driver maps at least the standard block up to and including the
/// 32-bit ADMA2 system-address register at [`REG_ADMA_ADDR`] (`0x58`),
/// so the DMA transfer path can program the descriptor-table base.
pub const REGS_LEN_BYTES: usize = 0x60;

// --- Register byte offsets (SDHCI standard block) -------------------------

/// `BLKSIZECNT`: block size `[15:0]` and block count `[31:16]`.
pub const REG_BLKSIZECNT: usize = 0x04;
/// `ARG1`: the 32-bit command argument.
pub const REG_ARG1: usize = 0x08;
/// `CMDTM`: transfer mode `[15:0]` and command `[31:16]`.
pub const REG_CMDTM: usize = 0x0C;
/// `RESP0`: command response word 0.
pub const REG_RESP0: usize = 0x10;
/// `RESP1`: command response word 1.
pub const REG_RESP1: usize = 0x14;
/// `RESP2`: command response word 2.
pub const REG_RESP2: usize = 0x18;
/// `RESP3`: command response word 3.
pub const REG_RESP3: usize = 0x1C;
/// `DATA`: the PIO buffer data port.
pub const REG_DATA: usize = 0x20;
/// `STATUS`: present-state register (line-busy / buffer-ready flags).
pub const REG_STATUS: usize = 0x24;
/// `CONTROL0`: host control `[7:0]`, power control `[15:8]`.
pub const REG_CONTROL0: usize = 0x28;
/// `CONTROL1`: clock control `[15:0]`, timeout `[19:16]`, reset `[26:24]`.
pub const REG_CONTROL1: usize = 0x2C;
/// `INTERRUPT`: normal interrupt status `[15:0]`, error status `[31:16]`
/// (write-1-to-clear).
pub const REG_INTERRUPT: usize = 0x30;
/// `IRPT_MASK`: interrupt-status enable bits.
pub const REG_IRPT_MASK: usize = 0x34;
/// `IRPT_EN`: interrupt-signal (to-CPU) enable bits.
pub const REG_IRPT_EN: usize = 0x38;
/// `ADMA_ADDR` (low 32 bits): the ARM-physical base of the 32-bit ADMA2
/// descriptor table the controller walks for a DMA transfer. Only the
/// low word is programmed; the driver drives 32-bit ADMA2, whose device
/// addresses fit the low 32 bits (the discovered DMA constraint bounds
/// them). The upper word (`0x5C`) is left zero.
pub const REG_ADMA_ADDR: usize = 0x58;

// --- `STATUS` (present state) bits ----------------------------------------

/// Command line is busy; a new command must not be issued.
pub const STATUS_CMD_INHIBIT: u32 = 1 << 0;
/// Data line is busy; a new data command must not be issued.
pub const STATUS_DAT_INHIBIT: u32 = 1 << 1;

// --- `CONTROL0` power-control bits (byte `[15:8]`) ------------------------

/// SD Bus Power: the card-supply rail is on. The standard register block
/// gates command/data activity on this bit, so it must be set before any
/// command is issued; a full host-controller reset clears it.
pub const CONTROL0_BUS_POWER: u32 = 1 << 8;
/// SD Bus Voltage Select = 3.3 V (the EMMC2-fed card rail). Occupies the
/// 3-bit voltage field `[11:9]` of the power-control byte.
pub const CONTROL0_BUS_VOLTAGE_3V3: u32 = 0b111 << 9;

// --- `CONTROL0` host-control bits (byte `[7:0]`) --------------------------

/// Data Transfer Width = 4-bit (`CONTROL0[1]`). Set after the card is
/// switched to the 4-bit bus with `ACMD6`, so the controller drives all
/// four DAT lines: a 4× transfer-rate improvement over the 1-bit reset
/// default. Cleared means the 1-bit bus.
pub const CONTROL0_DATA_WIDTH_4BIT: u32 = 1 << 1;

/// DMA Select field (`CONTROL0[4:3]`, SDHCI Host Control 1). The value
/// `0b10` selects 32-bit ADMA2, so a data command issued with
/// [`TM_DMA_EN`] makes the controller master the DAT-line data through
/// the ADMA2 descriptor table at [`REG_ADMA_ADDR`] rather than the
/// programmed-I/O buffer data port.
pub const CONTROL0_DMA_SELECT_ADMA2: u32 = 0b10 << 3;
/// Mask of the whole 2-bit DMA Select field (`CONTROL0[4:3]`), so the
/// field is cleared before the ADMA2 value is set in a read-modify-write.
pub const CONTROL0_DMA_SELECT_MASK: u32 = 0b11 << 3;

// --- `CONTROL1` bits ------------------------------------------------------

/// Internal clock enable.
pub const CONTROL1_CLK_INTLEN: u32 = 1 << 0;
/// Internal clock stable.
pub const CONTROL1_CLK_STABLE: u32 = 1 << 1;
/// SD clock enable.
pub const CONTROL1_CLK_EN: u32 = 1 << 2;
/// Reset the complete host controller.
pub const CONTROL1_SRST_HC: u32 = 1 << 24;

/// Bit offset of the 10-bit SD-clock frequency-select field (`[15:6]`).
pub const CONTROL1_CLK_FREQ_SHIFT: u32 = 8;
/// Bit offset of the data-timeout field (`[19:16]`).
pub const CONTROL1_TIMEOUT_SHIFT: u32 = 16;

// --- `INTERRUPT` bits (normal status, low half) ---------------------------

/// Command complete.
pub const INT_CMD_DONE: u32 = 1 << 0;
/// Data transfer complete.
pub const INT_DATA_DONE: u32 = 1 << 1;
/// Buffer write ready: the data port can accept a block.
pub const INT_WRITE_RDY: u32 = 1 << 4;
/// Buffer read ready: a block is available at the data port.
pub const INT_READ_RDY: u32 = 1 << 5;
/// An error interrupt is asserted; the error half `[31:16]` is set.
pub const INT_ERROR: u32 = 1 << 15;

/// Mask covering every error bit (the upper half of `INTERRUPT`).
pub const INT_ERROR_MASK: u32 = 0xFFFF_0000;

/// The `IRPT_EN` signal-enable mask the driver programs so the controller
/// asserts its CPU interrupt line on each completion the engine parks for
/// (a driver must wait on the interrupt, never busy-spin
/// a status register). It enables exactly the sources the engine waits on —
/// command complete, data-transfer complete, the PIO buffer-ready events —
/// plus every error bit, so a faulted transfer also wakes the parked task
/// rather than wedging it. The status-enable register (`IRPT_MASK`) latches
/// the same bits so the engine can read them back.
pub const INT_SIGNAL_ENABLE: u32 =
    INT_CMD_DONE | INT_DATA_DONE | INT_WRITE_RDY | INT_READ_RDY | INT_ERROR_MASK;

/// Every bit set: used to clear the whole `INTERRUPT` register
/// (write-1-to-clear) and to unmask every status bit.
pub const INT_ALL: u32 = 0xFFFF_FFFF;

// --- `CMDTM` command-register fields (upper half) -------------------------

/// Bit offset of the 6-bit command index (`[29:24]`).
pub const CMD_INDEX_SHIFT: u32 = 24;
/// Bit offset of the 2-bit response-type-select field (`[17:16]`).
pub const CMD_RESP_TYPE_SHIFT: u32 = 16;
/// Command uses CRC checking on its response (`[19]`).
pub const CMD_CRCCHK_EN: u32 = 1 << 19;
/// Command uses index checking on its response (`[20]`).
pub const CMD_IXCHK_EN: u32 = 1 << 20;
/// Command transfers data on the DAT lines (`[21]`).
pub const CMD_IS_DATA: u32 = 1 << 21;

/// Response-type select: no response.
pub const RESP_NONE: u32 = 0b00;
/// Response-type select: 136-bit response (R2).
pub const RESP_136: u32 = 0b01;
/// Response-type select: 48-bit response (R1/R3/R6/R7).
pub const RESP_48: u32 = 0b10;
/// Response-type select: 48-bit response with busy (R1b).
pub const RESP_48_BUSY: u32 = 0b11;

// --- `CMDTM` transfer-mode fields (lower half) ----------------------------

/// DMA-enable (`CMDTM` transfer mode `[0]`): the data phase is mastered
/// by the controller's DMA engine (ADMA2, selected by
/// [`CONTROL0_DMA_SELECT_ADMA2`]) instead of the programmed-I/O buffer
/// data port.
pub const TM_DMA_EN: u32 = 1 << 0;
/// Block-count-enable (multi-block transfers).
pub const TM_BLKCNT_EN: u32 = 1 << 1;
/// Auto-CMD12 enable (issue `STOP_TRANSMISSION` after a multi-block
/// transfer).
pub const TM_AUTO_CMD12: u32 = 0b01 << 2;
/// Data direction: card-to-host (read).
pub const TM_DAT_DIR_READ: u32 = 1 << 4;
/// Multi-block transfer.
pub const TM_MULTI_BLOCK: u32 = 1 << 5;
