//! SD-protocol command set, response shapes, and CSD decoding.
//!
//! These types describe the SD commands the driver issues and the
//! response register layout the SDHCI controller exposes. The block
//! geometry the driver reports is derived here from the card's CSD, never
//! assumed.

use tairix_abi::driver::block::BlockGeometry;
use tairix_abi::DriverError;

use crate::regs;

/// Fixed logical block size the driver operates on, in bytes.
///
/// High-capacity SD cards (SDHC/SDXC) are block-addressed at 512 bytes;
/// the driver sets this block length explicitly with `CMD16` and never
/// negotiates a larger one (`abi-v1`).
pub const BLOCK_SIZE: u32 = 512;

/// Number of 32-bit words in one 512-byte block at the PIO data port.
pub const BLOCK_WORDS: usize = BLOCK_SIZE as usize / 4;

/// How the controller returns a command's response.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ResponseKind {
    /// No response (e.g. `CMD0`).
    None,
    /// 48-bit response with CRC and index checking (R1/R6/R7).
    Short,
    /// 48-bit response that carries neither a valid CRC nor the command
    /// index, so both checks are disabled (R3 — the OCR register).
    ShortNoCrc,
    /// 48-bit response with a busy signal on the DAT line (R1b).
    ShortBusy,
    /// 136-bit response spanning `RESP0..RESP3` (R2 — CID/CSD).
    Long,
}

impl ResponseKind {
    /// The `CMDTM` response-type-select bits plus the CRC/index check
    /// bits the SD spec mandates for this response shape.
    pub(crate) const fn cmd_flags(self) -> u32 {
        match self {
            // No response: no checks (there is nothing to check).
            ResponseKind::None => regs::RESP_NONE << regs::CMD_RESP_TYPE_SHIFT,
            // R2 (136-bit): CRC checked, but the command index is not
            // echoed, so index checking is disabled.
            ResponseKind::Long => {
                (regs::RESP_136 << regs::CMD_RESP_TYPE_SHIFT) | regs::CMD_CRCCHK_EN
            }
            // R1/R6/R7 (48-bit): CRC and index both checked.
            ResponseKind::Short => {
                (regs::RESP_48 << regs::CMD_RESP_TYPE_SHIFT)
                    | regs::CMD_CRCCHK_EN
                    | regs::CMD_IXCHK_EN
            }
            // R3 (OCR): a 48-bit response that carries no CRC and no
            // command index, so both checks must be disabled or the
            // controller flags a spurious CRC error.
            ResponseKind::ShortNoCrc => regs::RESP_48 << regs::CMD_RESP_TYPE_SHIFT,
            ResponseKind::ShortBusy => {
                (regs::RESP_48_BUSY << regs::CMD_RESP_TYPE_SHIFT)
                    | regs::CMD_CRCCHK_EN
                    | regs::CMD_IXCHK_EN
            }
        }
    }
}

/// A single SD command the engine issues.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SdCommand {
    /// 6-bit command index (`CMDn`).
    pub index: u8,
    /// Expected response shape.
    pub response: ResponseKind,
    /// `true` when the command transfers data on the DAT lines (the
    /// direction is carried by the `CMDTM` transfer-mode half).
    pub transfers_data: bool,
}

impl SdCommand {
    const fn new(index: u8, response: ResponseKind) -> Self {
        Self {
            index,
            response,
            transfers_data: false,
        }
    }

    const fn data(index: u8, response: ResponseKind) -> Self {
        Self {
            index,
            response,
            transfers_data: true,
        }
    }

    /// Assemble the `CMDTM` command-register word (upper half) for this
    /// command, OR-ed with the response-class check bits.
    pub(crate) const fn cmd_word(self) -> u32 {
        let mut word = ((self.index as u32) << regs::CMD_INDEX_SHIFT) | self.response.cmd_flags();
        if self.transfers_data {
            word |= regs::CMD_IS_DATA;
        }
        word
    }
}

/// `CMD0` — `GO_IDLE_STATE`: reset every card to the idle state.
pub const GO_IDLE_STATE: SdCommand = SdCommand::new(0, ResponseKind::None);
/// `CMD2` — `ALL_SEND_CID`: every card returns its CID (R2).
pub const ALL_SEND_CID: SdCommand = SdCommand::new(2, ResponseKind::Long);
/// `CMD3` — `SEND_RELATIVE_ADDR`: the card publishes its RCA (R6).
pub const SEND_RELATIVE_ADDR: SdCommand = SdCommand::new(3, ResponseKind::Short);
/// `CMD7` — `SELECT_CARD`: select the addressed card (R1b).
pub const SELECT_CARD: SdCommand = SdCommand::new(7, ResponseKind::ShortBusy);
/// `CMD8` — `SEND_IF_COND`: voltage / pattern check (R7).
pub const SEND_IF_COND: SdCommand = SdCommand::new(8, ResponseKind::Short);
/// `CMD9` — `SEND_CSD`: the addressed card returns its CSD (R2).
pub const SEND_CSD: SdCommand = SdCommand::new(9, ResponseKind::Long);
/// `CMD16` — `SET_BLOCKLEN`: set the block length (R1).
pub const SET_BLOCKLEN: SdCommand = SdCommand::new(16, ResponseKind::Short);
/// `CMD17` — `READ_SINGLE_BLOCK` (R1, reads data).
pub const READ_SINGLE_BLOCK: SdCommand = SdCommand::data(17, ResponseKind::Short);
/// `CMD18` — `READ_MULTIPLE_BLOCK` (R1, reads data).
pub const READ_MULTIPLE_BLOCK: SdCommand = SdCommand::data(18, ResponseKind::Short);
/// `CMD24` — `WRITE_BLOCK` (R1, writes data).
pub const WRITE_BLOCK: SdCommand = SdCommand::data(24, ResponseKind::Short);
/// `CMD25` — `WRITE_MULTIPLE_BLOCK` (R1, writes data).
pub const WRITE_MULTIPLE_BLOCK: SdCommand = SdCommand::data(25, ResponseKind::Short);
/// `CMD55` — `APP_CMD`: the next command is an application command (R1).
pub const APP_CMD: SdCommand = SdCommand::new(55, ResponseKind::Short);
/// `ACMD41` — `SD_SEND_OP_COND`: negotiate the operating conditions (R3).
pub const SD_SEND_OP_COND: SdCommand = SdCommand::new(41, ResponseKind::ShortNoCrc);
/// `ACMD6` — `SET_BUS_WIDTH`: select the card's DAT-line bus width (R1).
pub const SET_BUS_WIDTH: SdCommand = SdCommand::new(6, ResponseKind::Short);

/// `ACMD6` argument selecting the 4-bit bus width (the 2-bit bus-width
/// field value `0b10`). The companion controller-side width bit is
/// [`regs::CONTROL0_DATA_WIDTH_4BIT`].
pub const BUS_WIDTH_4BIT_ARG: u32 = 0b10;

/// `CMD8` argument: 2.7–3.6 V supply (`0x1`) plus the `0xAA` check
/// pattern. The card echoes both in its R7 response.
pub const IF_COND_ARG: u32 = 0x0000_01AA;
/// Low byte of [`IF_COND_ARG`] — the check pattern the R7 must echo.
pub const IF_COND_CHECK_PATTERN: u32 = 0xAA;

/// `ACMD41` argument requesting a high-capacity (block-addressed) card at
/// the standard voltage window: HCS (`bit 30`) plus the 3.2–3.4 V bits.
pub const OP_COND_ARG: u32 = (1 << 30) | 0x00FF_8000;
/// `ACMD41` R3 bit 31: the card has finished its power-up sequence.
pub const OCR_READY: u32 = 1 << 31;
/// `ACMD41` R3 bit 30: Card Capacity Status — set means a block-addressed
/// high-capacity card.
pub const OCR_CCS: u32 = 1 << 30;

/// Decode the published Card Capacity Status (`CSD` v2, block-addressed)
/// into a [`BlockGeometry`].
///
/// `resp` holds the four R2 response words exactly as the SDHCI `RESP0..3`
/// registers report them: the 128-bit CSD with its 8-bit CRC tail
/// stripped, so `resp[n]` carries CSD bits `[32n+39 : 32n+8]`.
///
/// Only CSD structure version 2 (SDHC/SDXC) is supported: it block-
/// addresses the card, matching the [`OCR_CCS`] path the driver requires.
/// A version-1 (legacy standard-capacity) CSD is rejected with
/// [`DriverError::Unsupported`] rather than mis-decoded.
///
/// # Errors
///
/// * [`DriverError::Unsupported`] if the CSD is not structure version 2.
pub fn geometry_from_csd(resp: [u32; 4]) -> Result<BlockGeometry, DriverError> {
    // CSD_STRUCTURE is CSD bits [127:126] → response bits [119:118]. The
    // controller right-aligns the 120-bit (CRC-stripped) field across
    // `RESP0..3`, so `resp[3]` (`RESP3`) holds CSD[127:104] in its *low* 24
    // bits (`RESP3[31:24]` is zero padding above the field) and CSD[127:126]
    // sits at `resp[3]` bits [23:22], not the top of the word.
    let csd_structure = (resp[3] >> 22) & 0x3;
    if csd_structure != 1 {
        return Err(DriverError::Unsupported);
    }

    // C_SIZE (CSD v2) is CSD bits [69:48] → response bits [61:40], which
    // lie wholly within resp[1] (response bits [63:32]) at bits [29:8].
    let c_size = (resp[1] >> 8) & 0x003F_FFFF;

    // Capacity = (C_SIZE + 1) * 512 KiB = (C_SIZE + 1) * 1024 blocks of
    // 512 bytes (SD Physical Layer Spec §5.3.3).
    let block_count = (u64::from(c_size) + 1) * 1024;

    Ok(BlockGeometry {
        block_size: BLOCK_SIZE,
        block_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_word_encodes_index_and_response_class() {
        let word = READ_SINGLE_BLOCK.cmd_word();
        assert_eq!(word >> regs::CMD_INDEX_SHIFT & 0x3F, 17);
        assert_ne!(word & regs::CMD_IS_DATA, 0);
        assert_eq!(
            (word >> regs::CMD_RESP_TYPE_SHIFT) & 0b11,
            regs::RESP_48,
            "R1 is a 48-bit response"
        );

        let idle = GO_IDLE_STATE.cmd_word();
        assert_eq!(idle & regs::CMD_IS_DATA, 0);
        assert_eq!((idle >> regs::CMD_RESP_TYPE_SHIFT) & 0b11, regs::RESP_NONE);
    }

    #[test]
    fn csd_v2_decodes_capacity() {
        // C_SIZE = 0x3B37 → (0x3B37 + 1) * 1024 = 15,597,568 blocks
        // (~7.6 GiB), a representative 8 GB SDHC card. CSD_STRUCTURE = v2 is
        // CSD[127:126] = 01b, which the controller presents at `resp[3]`
        // bits [23:22] (see `geometry_from_csd`).
        let c_size: u32 = 0x3B37;
        let resp = [0, (c_size << 8), 0, 1 << 22];
        let geo = geometry_from_csd(resp).expect("v2 CSD decodes");
        assert_eq!(geo.block_size, 512);
        assert_eq!(geo.block_count, (u64::from(c_size) + 1) * 1024);
    }

    #[test]
    fn csd_v1_is_rejected() {
        // CSD_STRUCTURE = 0 (legacy standard capacity).
        let resp = [0, 0x1234_5600, 0, 0];
        assert_eq!(geometry_from_csd(resp), Err(DriverError::Unsupported));
    }

    #[test]
    fn csd_v2_minimum_capacity_is_one_unit() {
        // Structure v2 with C_SIZE = 0 yields the smallest legal
        // capacity, (0 + 1) * 1024 blocks, never zero.
        let resp = [0, 0, 0, 1 << 22];
        let geo = geometry_from_csd(resp).expect("minimum capacity");
        assert_eq!(geo.block_count, 1024);
    }

    #[test]
    fn structure_bits_above_the_field_are_not_read_as_v2() {
        // CSD_STRUCTURE lives at `resp[3]` bits [23:22]; the high byte of
        // `RESP3` is zero padding above the 120-bit field. A value placed
        // at the top of the word (the position a naive `resp[3] >> 30`
        // would have read) is *not* the structure field, so it must not be
        // mistaken for v2 — this is the metal CMD9 `SEND_CSD` regression
        // (`plans/PI.md` P8/B4), where the real card reported its v2
        // structure at [23:22] but the decoder looked at the wrong bits.
        let resp = [0, 0, 0, 1 << 30];
        assert_eq!(geometry_from_csd(resp), Err(DriverError::Unsupported));
    }
}
