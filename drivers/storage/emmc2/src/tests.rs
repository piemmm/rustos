//! Unit tests for the EMMC2 driver.
//!
//! [`MockSdhci`] is a register-level model of an SDHCI controller plus a
//! small backing card: it processes a written command, populates the
//! response registers, and feeds the buffer data port exactly as the
//! standard register block does. The [`Emmc2`] command/response and
//! block-transfer state machine is proven against it host-side (`AGENTS.md`
//! §2.2 — QEMU has no Pi EMMC2 model, `plans/PI.md` §0.4).
//!
//! [`MockMapper`] / [`MockHost`] cover the `wiring` capability gate; the
//! full register chain is proven through [`MockSdhci`], not a RAM-backed
//! window (which cannot model the controller).

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::ptr::NonNull;

use super::*;
use rustos_abi::driver::block::Block;
use rustos_abi::driver::DriverKind;
use rustos_abi::{CapabilityId, MmioMapError, MmioMapper, RegisterWindow};

use command::BLOCK_SIZE;

/// Backing-card size: 16 blocks of 512 bytes.
const STORE_BLOCKS: usize = 16;
const STORE_BYTES: usize = STORE_BLOCKS * BLOCK_SIZE as usize;

/// RCA the model publishes in its `CMD3` (R6) response.
const TEST_RCA: u32 = 0xAAAA;

/// A register-level SDHCI controller model with a small backing card.
///
/// The flags below model independent hardware/card conditions a test
/// toggles in isolation (capacity class, CSD version, command-error and
/// stall injection), so they are genuinely separate booleans rather than
/// a state enum — the `struct_excessive_bools` lint is allowed here for
/// that reason (`AGENTS.md` §15.10).
#[allow(clippy::struct_excessive_bools)]
struct MockSdhci {
    control1: u32,
    interrupt: u32,
    arg: u32,
    resp: [u32; 4],
    blksizecnt: u32,
    app_cmd: bool,

    // Card identity / capability the model presents.
    c_size: u32,
    high_capacity: bool,
    csd_structure_v2: bool,
    if_cond_echo: bool,
    acmd41_ready_after: u32,
    acmd41_count: u32,

    // PIO read state.
    store: [u8; STORE_BYTES],
    read_start: usize,
    read_cursor: usize,
    read_end: usize,

    // Fault injection.
    error_on_index: Option<u8>,
    stall: bool,
}

impl MockSdhci {
    /// A healthy high-capacity v2 card whose CSD reports `c_size`.
    fn healthy(c_size: u32) -> Self {
        Self {
            control1: 0,
            interrupt: 0,
            arg: 0,
            resp: [0; 4],
            blksizecnt: 0,
            app_cmd: false,
            c_size,
            high_capacity: true,
            csd_structure_v2: true,
            if_cond_echo: true,
            acmd41_ready_after: 2,
            acmd41_count: 0,
            store: [0u8; STORE_BYTES],
            read_start: 0,
            read_cursor: 0,
            read_end: 0,
            error_on_index: None,
            stall: false,
        }
    }

    /// Fill block `lba` with a deterministic per-block pattern.
    fn fill_block(&mut self, lba: usize, seed: u8) {
        let start = lba * BLOCK_SIZE as usize;
        let mut value = seed;
        for byte in &mut self.store[start..start + BLOCK_SIZE as usize] {
            *byte = value;
            value = value.wrapping_add(1);
        }
    }

    fn expected_block(seed: u8) -> Vec<u8> {
        let mut value = seed;
        (0..BLOCK_SIZE as usize)
            .map(|_| {
                let byte = value;
                value = value.wrapping_add(1);
                byte
            })
            .collect()
    }

    /// The four R2 response words encoding the model's CSD.
    fn csd_words(&self) -> [u32; 4] {
        let r3 = if self.csd_structure_v2 { 1 << 30 } else { 0 };
        let r1 = (self.c_size & 0x003F_FFFF) << 8;
        [0, r1, 0, r3]
    }

    /// Read the next data-port word, re-asserting `READ_RDY` at each
    /// block boundary and `DATA_DONE` when the transfer completes — the
    /// behaviour the real controller drives.
    fn next_data_word(&mut self) -> u32 {
        let off = self.read_cursor;
        let value = u32::from_le_bytes([
            self.store[off],
            self.store[off + 1],
            self.store[off + 2],
            self.store[off + 3],
        ]);
        self.read_cursor += 4;
        if (self.read_cursor - self.read_start) % BLOCK_SIZE as usize == 0 {
            if self.read_cursor < self.read_end {
                self.interrupt |= regs::INT_READ_RDY;
            } else {
                self.interrupt |= regs::INT_DATA_DONE;
            }
        }
        value
    }

    fn process_command(&mut self, cmdtm: u32) {
        let index = ((cmdtm >> regs::CMD_INDEX_SHIFT) & 0x3F) as u8;

        if self.stall {
            // Never assert command-complete: model an absent / wedged
            // controller so the engine's bounded wait fails closed.
            return;
        }
        if self.error_on_index == Some(index) {
            self.interrupt |= regs::INT_ERROR | (1 << 16);
            return;
        }

        let was_app = self.app_cmd;
        self.app_cmd = false;

        match index {
            8 => self.resp[0] = if self.if_cond_echo { self.arg } else { 0 },
            55 => {
                self.app_cmd = true;
                self.resp[0] = 0;
            }
            41 if was_app => {
                self.acmd41_count += 1;
                let mut ocr = 0x00FF_8000;
                if self.acmd41_count >= self.acmd41_ready_after {
                    ocr |= command::OCR_READY;
                    if self.high_capacity {
                        ocr |= command::OCR_CCS;
                    }
                }
                self.resp[0] = ocr;
            }
            2 => self.resp = [0x0102_0304, 0, 0, 0],
            3 => self.resp[0] = TEST_RCA << 16,
            9 => self.resp = self.csd_words(),
            7 | 16 => self.resp[0] = 0,
            17 | 18 => {
                let block_count = ((self.blksizecnt >> 16) & 0xFFFF) as usize;
                let start = self.arg as usize * BLOCK_SIZE as usize;
                self.read_start = start;
                self.read_cursor = start;
                self.read_end = start + block_count * BLOCK_SIZE as usize;
                self.resp[0] = 0;
                self.interrupt |= regs::INT_READ_RDY;
            }
            _ => {}
        }
        self.interrupt |= regs::INT_CMD_DONE;
    }
}

impl SdhciHost for MockSdhci {
    fn read32(&mut self, offset: usize) -> Result<u32, DriverError> {
        let value = match offset {
            regs::REG_CONTROL1 => {
                if self.control1 & regs::CONTROL1_CLK_INTLEN != 0 {
                    self.control1 | regs::CONTROL1_CLK_STABLE
                } else {
                    self.control1
                }
            }
            regs::REG_INTERRUPT => self.interrupt,
            regs::REG_RESP0 => self.resp[0],
            regs::REG_RESP1 => self.resp[1],
            regs::REG_RESP2 => self.resp[2],
            regs::REG_RESP3 => self.resp[3],
            regs::REG_DATA => self.next_data_word(),
            // Every other register (present state, IRPT enables, …) reads
            // as zero in the model: the engine only depends on the ones
            // above.
            _ => 0,
        };
        Ok(value)
    }

    fn write32(&mut self, offset: usize, value: u32) -> Result<(), DriverError> {
        match offset {
            // The host-controller reset bit self-clears once reset is
            // complete.
            regs::REG_CONTROL1 => self.control1 = value & !regs::CONTROL1_SRST_HC,
            // The interrupt register is write-1-to-clear.
            regs::REG_INTERRUPT => self.interrupt &= !value,
            regs::REG_ARG1 => self.arg = value,
            regs::REG_BLKSIZECNT => self.blksizecnt = value,
            regs::REG_CMDTM => self.process_command(value),
            _ => {}
        }
        Ok(())
    }
}

#[test]
fn open_runs_identification_and_reports_geometry() {
    let dev = Emmc2::open(MockSdhci::healthy(7)).expect("identification");
    let geo = dev.geometry().expect("geometry");
    assert_eq!(geo.block_size, 512);
    assert_eq!(geo.block_count, (7 + 1) * 1024);
}

#[test]
fn read_single_block_returns_card_data() {
    let mut mock = MockSdhci::healthy(7);
    mock.fill_block(3, 0x40);
    let mut dev = Emmc2::open(mock).expect("identification");

    let mut buf = [0u8; BLOCK_SIZE as usize];
    dev.read_blocks(3, &mut buf).expect("read");
    assert_eq!(buf.as_slice(), MockSdhci::expected_block(0x40).as_slice());
}

#[test]
fn read_multiple_blocks_returns_contiguous_data() {
    let mut mock = MockSdhci::healthy(7);
    mock.fill_block(1, 0x10);
    mock.fill_block(2, 0x50);
    mock.fill_block(3, 0x90);
    let mut dev = Emmc2::open(mock).expect("identification");

    let mut buf = [0u8; 3 * BLOCK_SIZE as usize];
    dev.read_blocks(1, &mut buf).expect("read");

    let bs = BLOCK_SIZE as usize;
    assert_eq!(&buf[0..bs], MockSdhci::expected_block(0x10).as_slice());
    assert_eq!(&buf[bs..2 * bs], MockSdhci::expected_block(0x50).as_slice());
    assert_eq!(
        &buf[2 * bs..3 * bs],
        MockSdhci::expected_block(0x90).as_slice()
    );
}

#[test]
fn read_rejects_short_and_misaligned_buffers() {
    let mut dev = Emmc2::open(MockSdhci::healthy(7)).expect("identification");
    let mut empty: [u8; 0] = [];
    assert_eq!(
        dev.read_blocks(0, &mut empty),
        Err(DriverError::BufferTooSmall)
    );
    let mut partial = [0u8; 200];
    assert_eq!(
        dev.read_blocks(0, &mut partial),
        Err(DriverError::BufferTooSmall)
    );
}

#[test]
fn read_rejects_out_of_range_lba() {
    let mut dev = Emmc2::open(MockSdhci::healthy(0)).expect("identification");
    // c_size 0 → 1024 blocks; LBA 1024 is one past the end.
    let mut buf = [0u8; BLOCK_SIZE as usize];
    assert_eq!(
        dev.read_blocks(1024, &mut buf),
        Err(DriverError::LengthOutOfRange)
    );
}

#[test]
fn byte_addressed_card_is_unsupported() {
    let mut mock = MockSdhci::healthy(7);
    mock.high_capacity = false; // ACMD41 reports CCS = 0.
    assert_eq!(Emmc2::open(mock).err(), Some(DriverError::Unsupported));
}

#[test]
fn non_v2_card_is_unsupported() {
    let mut mock = MockSdhci::healthy(7);
    // A card that does not echo CMD8's check pattern is pre-v2.
    mock.if_cond_echo = false;
    assert_eq!(Emmc2::open(mock).err(), Some(DriverError::Unsupported));
}

#[test]
fn csd_v1_card_is_unsupported() {
    let mut mock = MockSdhci::healthy(7);
    // High-capacity OCR but a structure-v1 CSD: rejected by the decoder
    // rather than mis-read.
    mock.csd_structure_v2 = false;
    assert_eq!(Emmc2::open(mock).err(), Some(DriverError::Unsupported));
}

#[test]
fn command_error_fails_closed() {
    let mut mock = MockSdhci::healthy(7);
    mock.fill_block(0, 0x11);
    // Inject an error response on the single-block read command.
    mock.error_on_index = Some(17);
    let mut dev = Emmc2::open(mock).expect("identification");
    let mut buf = [0u8; BLOCK_SIZE as usize];
    assert_eq!(dev.read_blocks(0, &mut buf), Err(DriverError::DeviceFault));
}

#[test]
fn stalled_controller_times_out_closed() {
    let mut mock = MockSdhci::healthy(7);
    mock.stall = true;
    // A tiny budget keeps the bounded wait quick; it must fail closed.
    assert_eq!(
        Emmc2::open_with_budget(mock, 8).err(),
        Some(DriverError::DeviceFault)
    );
}

#[test]
fn write_path_is_staged_read_only() {
    let mut dev = Emmc2::open(MockSdhci::healthy(7)).expect("identification");
    let payload = [0u8; BLOCK_SIZE as usize];
    assert_eq!(dev.write_blocks(0, &payload), Err(DriverError::Unsupported));
}

// --- `wiring` capability gate ---------------------------------------------

/// Minimal RAM-backed mapper for the `wiring` capability tests. The full
/// register chain is proven through [`MockSdhci`]; this only exercises the
/// capability / mapper gate (`AGENTS.md` §5.4).
struct MockMapper {
    phys: u64,
    backing: Vec<u32>,
    granted: bool,
}

impl MmioMapper for MockMapper {
    fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
        if !self.granted {
            return Err(MmioMapError::CapabilityMissing);
        }
        if phys_base != self.phys || len == 0 || len > self.backing.len() * 4 {
            return Err(MmioMapError::Unsupported);
        }
        let ptr = self.backing.as_ptr() as *mut u8;
        let base = NonNull::new(ptr).expect("non-null heap buffer");
        // SAFETY: `base` covers `backing.len() * 4 >= len` bytes and is
        // 4-byte aligned (the `Vec<u32>` allocation guarantee); the
        // backing outlives the window within the test, which never reads
        // it concurrently.
        Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len) })
    }
}

struct MockHost {
    drv_load: bool,
    mmio_map: bool,
    mapper: Option<MockMapper>,
}

impl DriverHost for MockHost {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        match cap {
            CapabilityId::DRV_LOAD => self.drv_load,
            CapabilityId::MMIO_MAP => self.mmio_map,
            _ => false,
        }
    }
    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }
    fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
        self.mapper.as_ref().map(|m| m as &dyn MmioMapper)
    }
}

const EMMC2_PHYS: u64 = 0xFE34_0000;

#[test]
fn register_requires_drv_load() {
    let granted = MockHost {
        drv_load: true,
        mmio_map: false,
        mapper: None,
    };
    assert!(register(&granted).is_ok());
    let denied = MockHost {
        drv_load: false,
        mmio_map: false,
        mapper: None,
    };
    assert_eq!(register(&denied), Err(DriverError::PermissionDenied));
}

#[test]
fn open_discovered_requires_mmio_map() {
    let host = MockHost {
        drv_load: true,
        mmio_map: false,
        mapper: Some(MockMapper {
            phys: EMMC2_PHYS,
            backing: vec![0u32; regs::REGS_LEN_BYTES / 4],
            granted: true,
        }),
    };
    assert_eq!(
        wiring::open_discovered(&host, EMMC2_PHYS).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn open_discovered_without_mapper_is_unsupported() {
    let host = MockHost {
        drv_load: true,
        mmio_map: true,
        mapper: None,
    };
    assert_eq!(
        wiring::open_discovered(&host, EMMC2_PHYS).err(),
        Some(DriverError::Unsupported)
    );
}
