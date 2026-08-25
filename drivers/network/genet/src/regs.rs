//! The BCM2711 GENET v5 register map and DMA descriptor layout.
//!
//! Offsets and bit fields are the published Broadcom GENET register map (as
//! carried by the Linux `bcmgenet` and U-Boot `bcmgenet` drivers). Everything
//! is expressed relative to the single register aperture the discovered
//! `brcm,bcm2711-genet-v5` node names — the driver never bakes in a base
//! address.
//!
//! # Blocks
//!
//! | Block      | Offset   | Role                                        |
//! |------------|----------|---------------------------------------------|
//! | `SYS`      | `0x0000` | core revision, port mode, RBUF/TBUF flush   |
//! | `EXT`      | `0x0080` | RGMII out-of-band control                   |
//! | `INTRL2_0` | `0x0200` | level-2 interrupt controller (DMA + link)   |
//! | `RBUF`     | `0x0300` | receive buffer control                      |
//! | `UMAC`     | `0x0800` | UniMAC: command, address, MIB, MDIO         |
//! | `RDMA`     | `0x2000` | receive descriptor RAM, then ring registers |
//! | `TDMA`     | `0x4000` | transmit descriptor RAM, then ring regs      |
//!
//! Both DMA blocks are laid out the same way: [`TOTAL_DESC`] descriptors of
//! [`DESC_WORDS`] words, then one [`RING_STRIDE`]-byte register block per
//! ring, then the block's own control registers. The driver drives the
//! default ring ([`DEFAULT_RING`]) only.

/// Total descriptors the on-chip descriptor RAM holds per direction. The
/// hardware's fixed geometry: it fixes where the per-ring registers begin,
/// independently of how many descriptors a ring is programmed to use.
pub const TOTAL_DESC: u32 = 256;

/// Words in one DMA descriptor (`length_status`, `address_lo`,
/// `address_hi`) — GENET v4+ descriptors are 3 words wide (40-bit
/// addressing).
pub const DESC_WORDS: u32 = 3;

/// Bytes in one DMA descriptor.
pub const DESC_LEN: usize = (DESC_WORDS * 4) as usize;

/// Byte stride between two rings' register blocks.
pub const RING_STRIDE: usize = 0x40;

/// The ring the driver drives: ring 16, the descriptor-based default queue
/// (rings 0..15 are the priority queues this driver does not use).
pub const DEFAULT_RING: u32 = 16;

/// Descriptor-word offset within a descriptor: length and status.
pub const DESC_LENGTH_STATUS: usize = 0x00;
/// Descriptor-word offset within a descriptor: low 32 bits of the buffer's
/// device-visible address.
pub const DESC_ADDRESS_LO: usize = 0x04;
/// Descriptor-word offset within a descriptor: high bits of the buffer's
/// device-visible address.
pub const DESC_ADDRESS_HI: usize = 0x08;

// --- SYS block ----------------------------------------------------------

/// Base of the `SYS` block.
pub const SYS: usize = 0x0000;

/// `SYS_REV_CTRL`: core revision. Bits 27..24 carry the major-revision
/// encoding ([`GENET_V5_MAJOR`]), 19..16 the minor, 15..0 the patch level.
pub const SYS_REV_CTRL: usize = SYS;

/// The major-revision nibble a GENET **v5** core reports in
/// [`SYS_REV_CTRL`]. The encoding is offset from the marketing version (v5
/// reports `6`, v4 reports `5`), so this is the only value this driver
/// accepts — a core reporting anything else is not the device the matched
/// node claimed and bring-up fails closed rather than programming a
/// register layout that may not be there.
pub const GENET_V5_MAJOR: u32 = 6;

/// Shift of the major-revision nibble in [`SYS_REV_CTRL`].
pub const REV_MAJOR_SHIFT: u32 = 24;
/// Mask of the major-revision nibble, after shifting.
pub const REV_MAJOR_MASK: u32 = 0x0F;

/// `SYS_PORT_CTRL`: selects the MAC's external interface mode.
pub const SYS_PORT_CTRL: usize = SYS + 0x04;

/// Port mode: external gigabit PHY over RGMII — the Pi 4's BCM54213PE.
pub const PORT_MODE_EXT_GPHY: u32 = 3;

/// `SYS_RBUF_FLUSH_CTRL`: bit 1 asserts a receive-buffer reset while set.
pub const SYS_RBUF_FLUSH_CTRL: usize = SYS + 0x08;

/// The receive-buffer reset bit in [`SYS_RBUF_FLUSH_CTRL`].
pub const RBUF_FLUSH_RESET: u32 = 1 << 1;

/// `SYS_TBUF_FLUSH_CTRL`: the transmit-buffer counterpart.
pub const SYS_TBUF_FLUSH_CTRL: usize = SYS + 0x0C;

// --- EXT block ----------------------------------------------------------

/// Base of the `EXT` block.
pub const EXT: usize = 0x0080;

/// `EXT_RGMII_OOB_CTRL`: RGMII out-of-band link/mode control.
pub const EXT_RGMII_OOB_CTRL: usize = EXT + 0x0C;

/// Drive the RGMII link-status indication from this register rather than
/// the PHY's in-band signalling.
pub const RGMII_LINK: u32 = 1 << 4;
/// When set, the out-of-band link/speed indication is ignored. Cleared once
/// the driver programs the negotiated link.
pub const OOB_DISABLE: u32 = 1 << 5;
/// Enable RGMII mode on the external interface.
pub const RGMII_MODE_EN: u32 = 1 << 6;
/// Disable the internal RGMII transmit-clock delay: the Pi 4 wires
/// `rgmii-rxid`, so the *receive* delay is added by the PHY and the MAC must
/// not add a transmit-side delay of its own.
pub const ID_MODE_DIS: u32 = 1 << 16;

// --- INTRL2_0: the level-2 interrupt controller -------------------------

/// Base of the `INTRL2_0` block — the interrupt instance carrying the
/// default ring's DMA completions and the link events.
pub const INTRL2_0: usize = 0x0200;

/// Asserted interrupt sources (read-only).
pub const INTRL2_CPU_STAT: usize = INTRL2_0;
/// Write-1-to-clear of an asserted source.
pub const INTRL2_CPU_CLEAR: usize = INTRL2_0 + 0x08;
/// Write-1-to-mask (disable) a source.
pub const INTRL2_CPU_MASK_SET: usize = INTRL2_0 + 0x10;
/// Write-1-to-unmask (enable) a source.
pub const INTRL2_CPU_MASK_CLEAR: usize = INTRL2_0 + 0x14;

/// Every source bit, for the wholesale mask-all at bring-up.
pub const INTRL2_ALL: u32 = 0xFFFF_FFFF;

/// The PHY reported the link came up.
pub const IRQ_LINK_UP: u32 = 1 << 4;
/// The PHY reported the link went down.
pub const IRQ_LINK_DOWN: u32 = 1 << 5;
/// The receive DMA engine finished a burst of buffers.
pub const IRQ_RXDMA_DONE: u32 = 1 << 12;
/// The transmit DMA engine finished a burst of buffers.
pub const IRQ_TXDMA_DONE: u32 = 1 << 15;

/// The sources the driver enables: the two DMA-completion doorbells the
/// serve loop needs plus both link events, so a cable change wakes the
/// driver and the stack re-reads the link state. Every other source stays
/// masked — an unhandled condition must not be able to re-assert the line
/// faster than it can be serviced.
pub const IRQ_ENABLED: u32 = IRQ_RXDMA_DONE | IRQ_TXDMA_DONE | IRQ_LINK_UP | IRQ_LINK_DOWN;

// --- RBUF block ---------------------------------------------------------

/// Base of the `RBUF` block.
pub const RBUF: usize = 0x0300;

/// `RBUF_CTRL`: receive-buffer behaviour.
pub const RBUF_CTRL: usize = RBUF;

/// Insert two pad bytes before every received frame, so the IP header lands
/// 4-byte aligned. The reported descriptor length *includes* the pad, so the
/// frame itself starts [`RX_FRAME_OFFSET`] bytes into the buffer.
pub const RBUF_ALIGN_2B: u32 = 1 << 1;

/// Bytes of receive-buffer padding [`RBUF_ALIGN_2B`] inserts.
pub const RX_FRAME_OFFSET: u32 = 2;

/// `RBUF_TBUF_SIZE_CTRL`: transmit-buffer size selector; `1` is the
/// single-port setting the Pi 4's one MAC uses.
pub const RBUF_TBUF_SIZE_CTRL: usize = RBUF + 0xB4;

/// The single-port transmit-buffer size selector.
pub const TBUF_SIZE_ONE_PORT: u32 = 1;

// --- UMAC block ---------------------------------------------------------

/// Base of the `UMAC` (UniMAC) block.
pub const UMAC: usize = 0x0800;

/// `UMAC_CMD`: MAC enable, speed, and reset control.
pub const UMAC_CMD: usize = UMAC + 0x008;

/// Enable the transmitter.
pub const CMD_TX_EN: u32 = 1 << 0;
/// Enable the receiver.
pub const CMD_RX_EN: u32 = 1 << 1;
/// Shift of the two-bit link-speed selector in [`UMAC_CMD`].
pub const CMD_SPEED_SHIFT: u32 = 2;
/// Mask of the link-speed selector, after shifting.
pub const CMD_SPEED_MASK: u32 = 0x3;
/// Assert the MAC's software reset.
pub const CMD_SW_RESET: u32 = 1 << 13;
/// Loop the transmitter back into the receiver — held with
/// [`CMD_SW_RESET`] during the reset pulse so no partial frame reaches the
/// wire while the MAC is being reset.
pub const CMD_LCL_LOOP_EN: u32 = 1 << 15;

/// [`UMAC_CMD`] speed selector: 10 Mb/s.
pub const UMAC_SPEED_10: u32 = 0;
/// [`UMAC_CMD`] speed selector: 100 Mb/s.
pub const UMAC_SPEED_100: u32 = 1;
/// [`UMAC_CMD`] speed selector: 1000 Mb/s.
pub const UMAC_SPEED_1000: u32 = 2;

/// `UMAC_MAC0`: the link-layer address's first four octets, big-endian in
/// the register (octet 0 in the most-significant byte).
pub const UMAC_MAC0: usize = UMAC + 0x00C;
/// `UMAC_MAC1`: the address's last two octets in the low half-word.
pub const UMAC_MAC1: usize = UMAC + 0x010;

/// `UMAC_MAX_FRAME_LEN`: longest frame the receiver accepts.
pub const UMAC_MAX_FRAME_LEN: usize = UMAC + 0x014;

/// `UMAC_MIB_CTRL`: statistics-counter resets.
pub const UMAC_MIB_CTRL: usize = UMAC + 0x580;

/// Reset the receive statistics counters.
pub const MIB_RESET_RX: u32 = 1 << 0;
/// Reset the runt-frame counter.
pub const MIB_RESET_RUNT: u32 = 1 << 1;
/// Reset the transmit statistics counters.
pub const MIB_RESET_TX: u32 = 1 << 2;

/// `UMAC_TX_FLUSH`: drains the transmit path while set.
pub const UMAC_TX_FLUSH: usize = UMAC + 0x334;

/// `UMAC_MDF_CTRL`: per-slot enables for the receive destination-address
/// filter. Slot `n` is enabled by bit ([`MDF_SLOTS`] `- 1 - n`); a frame
/// whose destination matches no enabled slot is dropped by the MAC, so a
/// controller left with this register clear receives nothing at all.
pub const UMAC_MDF_CTRL: usize = UMAC + 0x650;

/// Base of the destination-address filter slots. Each slot spans two
/// registers — octets 0..2 in the low half-word of the first, octets 2..6 in
/// the second — so slot `n` starts [`MDF_SLOT_STRIDE`]`* n` bytes in.
pub const UMAC_MDF_ADDR: usize = UMAC + 0x654;

/// Destination-address filter slots the UniMAC provides.
pub const MDF_SLOTS: u16 = 17;

/// Bytes one destination-address filter slot occupies.
pub const MDF_SLOT_STRIDE: usize = 8;

/// `MDIO_CMD`: the UniMAC's clause-22 MDIO master.
pub const MDIO_CMD: usize = UMAC + 0x614;

/// Set to start a transaction; the controller clears it on completion.
pub const MDIO_START_BUSY: u32 = 1 << 29;
/// Set by the controller when a read found no responding PHY.
pub const MDIO_READ_FAIL: u32 = 1 << 28;
/// Transaction opcode: read.
pub const MDIO_RD: u32 = 2 << 26;
/// Transaction opcode: write.
pub const MDIO_WR: u32 = 1 << 26;
/// Shift of the PHY (PMD) address.
pub const MDIO_PMD_SHIFT: u32 = 21;
/// Mask of the PHY address, before shifting.
pub const MDIO_PMD_MASK: u32 = 0x1F;
/// Shift of the register number.
pub const MDIO_REG_SHIFT: u32 = 16;
/// Mask of the register number, before shifting.
pub const MDIO_REG_MASK: u32 = 0x1F;
/// Mask of the 16-bit data half-word.
pub const MDIO_DATA_MASK: u32 = 0xFFFF;

// --- Descriptor status/control bits -------------------------------------

/// Shift of the buffer/frame length in a descriptor's `length_status`.
pub const DMA_BUFLENGTH_SHIFT: u32 = 16;
/// Mask of the buffer/frame length, after shifting.
pub const DMA_BUFLENGTH_MASK: u32 = 0x0FFF;
/// The descriptor is owned by the device.
pub const DMA_OWN: u32 = 0x8000;
/// This descriptor holds the end of a frame.
pub const DMA_EOP: u32 = 0x4000;
/// This descriptor holds the start of a frame.
pub const DMA_SOP: u32 = 0x2000;
/// Last descriptor of the ring: the engine wraps after it.
pub const DMA_WRAP: u32 = 0x1000;

/// Transmit: have the MAC append the frame check sequence.
pub const DMA_TX_APPEND_CRC: u32 = 0x0040;
/// Shift of the transmit queue tag.
pub const DMA_TX_QTAG_SHIFT: u32 = 7;
/// The queue-tag value for an untagged frame on GENET v5 (the full 6-bit
/// mask — the switch-tag pass-through encoding).
pub const DMA_TX_QTAG_MASK: u32 = 0x3F;

/// Receive: frame longer than the configured maximum.
pub const DMA_RX_LG: u32 = 0x0010;
/// Receive: frame was not an integral number of octets.
pub const DMA_RX_NO: u32 = 0x0008;
/// Receive: the MAC flagged a receive error.
pub const DMA_RX_RXER: u32 = 0x0004;
/// Receive: frame check sequence mismatch.
pub const DMA_RX_CRC_ERROR: u32 = 0x0002;
/// Receive: the receive buffer overflowed.
pub const DMA_RX_OV: u32 = 0x0001;

/// Every receive error bit: a descriptor carrying any of them is dropped
/// whole rather than delivered.
pub const DMA_RX_ERRORS: u32 = DMA_RX_LG | DMA_RX_NO | DMA_RX_RXER | DMA_RX_CRC_ERROR | DMA_RX_OV;

// --- Per-ring registers -------------------------------------------------

/// Ring register: transmit read pointer / receive write pointer.
pub const RING_RW_POINTER: usize = 0x00;
/// Ring register: transmit consumer index / receive **producer** index —
/// the device's own counter.
pub const RING_DEVICE_INDEX: usize = 0x08;
/// Ring register: transmit **producer** index / receive consumer index —
/// the driver's counter.
pub const RING_DRIVER_INDEX: usize = 0x0C;
/// Ring register: descriptor count (high half) and buffer size (low half).
pub const RING_BUF_SIZE: usize = 0x10;
/// Ring register: first descriptor word of the ring.
pub const RING_START_ADDR: usize = 0x14;
/// Ring register: high half of the first descriptor word.
pub const RING_START_ADDR_HI: usize = 0x18;
/// Ring register: last descriptor word of the ring.
pub const RING_END_ADDR: usize = 0x1C;
/// Ring register: high half of the last descriptor word.
pub const RING_END_ADDR_HI: usize = 0x20;
/// Ring register: buffers-done interrupt threshold.
pub const RING_MBUF_DONE_THRESH: usize = 0x24;
/// Ring register: transmit flow-control period / receive XON-XOFF
/// thresholds.
pub const RING_FLOW_PERIOD: usize = 0x28;
/// Ring register: transmit write pointer / receive read pointer.
pub const RING_WR_POINTER: usize = 0x2C;

/// Shift of the descriptor count in [`RING_BUF_SIZE`].
pub const RING_SIZE_SHIFT: u32 = 16;

/// Producer/consumer indices are 16-bit free-running counters; the rest of
/// the register carries a discard/done count the driver does not read.
pub const RING_INDEX_MASK: u32 = 0xFFFF;

// --- Per-block DMA control registers ------------------------------------

/// Control register: per-ring enable bitmap.
pub const DMA_RING_CFG: usize = 0x00;
/// Control register: engine enable plus the per-ring buffer enables.
pub const DMA_CTRL: usize = 0x04;
/// Control register: system-bus burst size.
pub const DMA_SCB_BURST_SIZE: usize = 0x0C;

/// Enable the DMA engine.
pub const DMA_EN: u32 = 1 << 0;
/// Shift of the per-ring buffer-enable bitmap in [`DMA_CTRL`].
pub const DMA_RING_BUF_EN_SHIFT: u32 = 1;
/// Burst length (in 64-byte units) the engine requests on the system bus.
pub const DMA_MAX_BURST_LENGTH: u32 = 0x08;

/// Receive flow-control high threshold: back-pressure once this many
/// descriptors are outstanding.
const DMA_FC_THRESH_HI: u32 = TOTAL_DESC >> 4;
/// Receive flow-control low threshold.
const DMA_FC_THRESH_LO: u32 = 5;
/// The packed XON/XOFF threshold pair written to [`RING_FLOW_PERIOD`] on
/// the receive ring.
pub const DMA_FC_THRESH: u32 = (DMA_FC_THRESH_LO << 16) | DMA_FC_THRESH_HI;

/// Base of the receive descriptor RAM.
pub const RDMA_DESC: usize = 0x2000;
/// Base of the transmit descriptor RAM.
pub const TDMA_DESC: usize = 0x4000;

/// Byte offset of descriptor `index` within a descriptor RAM at `desc_base`.
#[must_use]
pub const fn desc(desc_base: usize, index: u32) -> usize {
    desc_base + index as usize * DESC_LEN
}

/// Byte offset of ring `ring`'s register block for the DMA engine whose
/// descriptor RAM starts at `desc_base`. The per-ring blocks follow the
/// full descriptor RAM, whatever the ring is programmed to use.
#[must_use]
pub const fn ring_regs(desc_base: usize, ring: u32) -> usize {
    desc_base + TOTAL_DESC as usize * DESC_LEN + ring as usize * RING_STRIDE
}

/// Byte offset of the block-wide DMA control registers for the engine whose
/// descriptor RAM starts at `desc_base`: they follow all
/// `DEFAULT_RING + 1` per-ring blocks.
#[must_use]
pub const fn dma_regs(desc_base: usize) -> usize {
    desc_base + TOTAL_DESC as usize * DESC_LEN + (DEFAULT_RING as usize + 1) * RING_STRIDE
}
