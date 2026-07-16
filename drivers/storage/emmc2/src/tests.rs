//! Unit tests for the EMMC2 driver.
//!
//! [`MockSdhci`] is a register-level model of an SDHCI controller plus a
//! small backing card: it processes a written command, populates the
//! response registers, and moves data through the buffer data port in
//! both directions exactly as the standard register block does. The
//! [`Emmc2`] command/response and block-transfer state machine is proven
//! against it host-side (QEMU has no Pi EMMC2 model,
//! `plans/PI.md` §0.4).
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

/// Synthetic device-visible base the DMA-capable model reports for its
/// staging region. Non-zero (a real mapping never bases at 0) and well
/// within the 32-bit ADMA2 address field so the descriptor/table addresses
/// the engine programs are representable.
const DMA_DEVICE_BASE: u64 = 0x8000_0000;

/// A register-level SDHCI controller model with a small backing card.
///
/// The flags below model independent hardware/card conditions a test
/// toggles in isolation (capacity class, CSD version, command-error and
/// stall injection), so they are genuinely separate booleans rather than
/// a state enum — the `struct_excessive_bools` lint is allowed here for
/// that reason.
#[allow(clippy::struct_excessive_bools)]
struct MockSdhci {
    control0: u32,
    control1: u32,
    interrupt: u32,
    arg: u32,
    resp: [u32; 4],
    blksizecnt: u32,
    app_cmd: bool,

    // SD Bus Power state. `power_on` tracks the power-control byte the
    // driver writes; the controller refuses to complete commands while the
    // bus is dark. `power_wired` models a rail that cannot come up (the
    // power write is honoured only when wired), so a regression test can
    // prove the engine depends on the power-on write.
    power_on: bool,
    power_wired: bool,

    // The `ACMD6 SET_BUS_WIDTH` argument the engine last issued (the
    // 4-bit-bus speed step), so a test can assert the card was switched to
    // the 4-bit bus.
    acmd6_arg: Option<u32>,

    // Card identity / capability the model presents.
    c_size: u32,
    high_capacity: bool,
    csd_structure_v2: bool,
    if_cond_echo: bool,
    acmd41_ready_after: u32,
    acmd41_count: u32,

    // Backing card.
    store: Vec<u8>,

    // PIO transfer state.
    read_start: usize,
    read_cursor: usize,
    read_end: usize,
    write_start: usize,
    write_cursor: usize,
    write_end: usize,

    // ADMA2 DMA model. `dma_capable` makes `dma_region` hand the engine a
    // staging region so it drives the DMA path; `dma_buf` is that region
    // (data area then one-entry descriptor table, `DMA_REGION_BYTES`),
    // `dma_base` its synthetic device-visible base, and `adma_addr` the
    // descriptor-table address the engine last programmed.
    dma_capable: bool,
    dma_buf: Vec<u8>,
    dma_base: u64,
    adma_addr: u32,
    dma_syncs: Vec<(usize, usize)>,

    // Fault injection.
    error_on_index: Option<u8>,
    stall: bool,

    // Interrupt-delivery model. With `defer` set, completion bits are
    // *staged* by the controller and only become visible in `INTERRUPT`
    // when `await_irq` runs — i.e. the engine cannot observe a completion
    // without parking on the interrupt. `await_calls` counts those parks so
    // a regression test can prove the engine is interrupt-driven and never
    // busy-spins a status register.
    defer: bool,
    staged: u32,
    await_calls: u32,

    // Dead-interrupt model: every `await_irq` reports a timed-out wait
    // (no completion and no error ever signalled), modelling a dead
    // controller or broken interrupt routing.
    silent: bool,

    // When set, `write32` asserts the driver programs `IRPT_EN` with the
    // completion + error signal-enable mask, guarding
    // the regression where a zero `IRPT_EN` left the interrupt line dead.
    assert_irpt_en: bool,
}

impl MockSdhci {
    /// A healthy high-capacity v2 card whose CSD reports `c_size`.
    fn healthy(c_size: u32) -> Self {
        Self {
            control0: 0,
            control1: 0,
            interrupt: 0,
            arg: 0,
            resp: [0; 4],
            blksizecnt: 0,
            app_cmd: false,
            power_on: false,
            power_wired: true,
            acmd6_arg: None,
            c_size,
            high_capacity: true,
            csd_structure_v2: true,
            if_cond_echo: true,
            acmd41_ready_after: 2,
            acmd41_count: 0,
            store: vec![0u8; STORE_BYTES],
            read_start: 0,
            read_cursor: 0,
            read_end: 0,
            write_start: 0,
            write_cursor: 0,
            write_end: 0,
            dma_capable: false,
            dma_buf: vec![0u8; DMA_REGION_BYTES],
            dma_base: DMA_DEVICE_BASE,
            adma_addr: 0,
            dma_syncs: Vec::new(),
            error_on_index: None,
            stall: false,
            defer: false,
            silent: false,
            staged: 0,
            await_calls: 0,
            assert_irpt_en: false,
        }
    }

    /// A healthy card that delivers every completion only through its
    /// interrupt: completion bits are staged and revealed to `INTERRUPT`
    /// when the engine parks on [`SdhciHost::await_irq`]. Proves the engine
    /// is interrupt-driven and never busy-spins.
    fn healthy_deferred(c_size: u32) -> Self {
        Self {
            defer: true,
            ..Self::healthy(c_size)
        }
    }

    /// A healthy high-capacity v2 card that offers the engine a DMA staging
    /// region, so bring-up selects the ADMA2 transfer path. The backing
    /// card is `store_blocks` blocks so a transfer larger than one DMA
    /// chunk ([`DMA_STAGE_BLOCKS`]) can be exercised.
    fn healthy_dma(c_size: u32, store_blocks: usize) -> Self {
        Self {
            dma_capable: true,
            store: vec![0u8; store_blocks * BLOCK_SIZE as usize],
            ..Self::healthy(c_size)
        }
    }

    /// Fill `count` consecutive blocks from `lba` with the per-block
    /// pattern `expected_block(seed + n)` gives, so a multi-block DMA read
    /// can be checked against a known pattern.
    fn fill_blocks(&mut self, lba: usize, count: usize, seed: u8) {
        for n in 0..count {
            self.fill_block(
                lba + n,
                seed.wrapping_add(u8::try_from(n % 256).unwrap_or(0)),
            );
        }
    }

    /// Perform one ADMA2 transfer: walk the one-entry descriptor table the
    /// engine programmed at `adma_addr` and move its `Length` bytes between
    /// the backing store (at the block address in `arg`) and the descriptor's
    /// data address, then raise transfer-complete.
    ///
    /// This is the register-level model of the controller's DMA engine: it
    /// reads the descriptor the driver staged in `dma_buf`, so a bug in the
    /// driver's descriptor encoding, block count, or address arithmetic
    /// shows up as wrong or missing data rather than passing silently.
    fn process_dma_transfer(&mut self, is_read: bool) {
        // The controller must have been switched to ADMA2 before a DMA
        // command — the driver's `enable_adma2` does this at bring-up.
        assert_eq!(
            self.control0 & regs::CONTROL0_DMA_SELECT_MASK,
            regs::CONTROL0_DMA_SELECT_ADMA2,
            "a DMA command requires ADMA2 selected in CONTROL0",
        );
        let table_off = usize::try_from(u64::from(self.adma_addr) - self.dma_base)
            .expect("descriptor-table offset fits usize");
        let desc = &self.dma_buf[table_off..table_off + adma::DESC_BYTES];
        let attr = u16::from_le_bytes([desc[0], desc[1]]);
        assert_ne!(attr & 0x1, 0, "descriptor Valid");
        assert_ne!(attr & 0x2, 0, "descriptor End (one-entry table)");
        assert_eq!(attr & (0b11 << 4), 0b10 << 4, "descriptor Act = Tran");
        let length = u16::from_le_bytes([desc[2], desc[3]]);
        let len_bytes = if length == 0 {
            adma::MAX_DESC_BYTES
        } else {
            length as usize
        };
        let data_addr = u32::from_le_bytes([desc[4], desc[5], desc[6], desc[7]]);
        let data_off =
            usize::try_from(u64::from(data_addr) - self.dma_base).expect("data offset fits usize");

        // The block count the engine programmed must match the descriptor.
        let block_count = ((self.blksizecnt >> 16) & 0xFFFF) as usize;
        assert_eq!(
            len_bytes,
            block_count * BLOCK_SIZE as usize,
            "descriptor length matches the programmed block count",
        );

        let card_off = self.arg as usize * BLOCK_SIZE as usize;
        if is_read {
            let (data, store) = (&mut self.dma_buf, &self.store);
            data[data_off..data_off + len_bytes]
                .copy_from_slice(&store[card_off..card_off + len_bytes]);
        } else {
            let data = &self.dma_buf[data_off..data_off + len_bytes];
            self.store[card_off..card_off + len_bytes].copy_from_slice(data);
        }
        // A DMA transfer completes with a single transfer-complete
        // interrupt — no per-block buffer-ready handshake.
        self.raise(regs::INT_DATA_DONE);
    }

    /// Raise completion `bits`: visible immediately in normal mode, or
    /// staged until the next `await_irq` in the interrupt-delivery model.
    fn raise(&mut self, bits: u32) {
        if self.defer {
            self.staged |= bits;
        } else {
            self.interrupt |= bits;
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

    /// The four R2 response words encoding the model's CSD, laid out exactly
    /// as the SDHCI controller presents `RESP0..3`: the 120-bit CRC-stripped
    /// field is right-aligned, so `CSD_STRUCTURE` (CSD[127:126]) lands at
    /// `RESP3` bits [23:22] and `C_SIZE` at `RESP1` bits [29:8] (`command.rs`
    /// `geometry_from_csd`). Encoding the structure at the wrong position is
    /// the bug that let the metal CMD9 `SEND_CSD` failure escape host tests
    /// (`plans/PI.md` P8/B4), so the model mirrors the real layout.
    fn csd_words(&self) -> [u32; 4] {
        let r3 = if self.csd_structure_v2 { 1 << 22 } else { 0 };
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
                self.raise(regs::INT_READ_RDY);
            } else {
                self.raise(regs::INT_DATA_DONE);
            }
        }
        value
    }

    /// Accept a data-port word into the backing store, re-asserting
    /// `WRITE_RDY` at each block boundary and `DATA_DONE` when the
    /// transfer completes — the behaviour the real controller drives.
    fn accept_data_word(&mut self, value: u32) {
        if self.write_cursor >= self.write_end {
            // A data-port write outside an active transfer is dropped,
            // as the real controller's buffer logic does.
            return;
        }
        let off = self.write_cursor;
        self.store[off..off + 4].copy_from_slice(&value.to_le_bytes());
        self.write_cursor += 4;
        if (self.write_cursor - self.write_start) % BLOCK_SIZE as usize == 0 {
            if self.write_cursor < self.write_end {
                self.raise(regs::INT_WRITE_RDY);
            } else {
                self.raise(regs::INT_DATA_DONE);
            }
        }
    }

    fn process_command(&mut self, cmdtm: u32) {
        let index = ((cmdtm >> regs::CMD_INDEX_SHIFT) & 0x3F) as u8;

        if !self.power_on {
            // The SD bus is unpowered: the controller drives nothing on the
            // line and command-complete never asserts, so the engine's
            // bounded wait fails closed. This is the exact metal symptom the
            // missing power-on write produced (`plans/PI.md` P8/B4).
            return;
        }
        if self.stall {
            // Never assert command-complete: model an absent / wedged
            // controller so the engine's bounded wait fails closed.
            return;
        }
        if self.error_on_index == Some(index) {
            self.raise(regs::INT_ERROR | (1 << 16));
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
            6 if was_app => {
                // ACMD6 SET_BUS_WIDTH: record the requested width so a
                // test can assert the 4-bit switch; the R1 response is 0.
                self.acmd6_arg = Some(self.arg);
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
                self.resp[0] = 0;
                if cmdtm & regs::TM_DMA_EN != 0 {
                    // ADMA2 read: the controller masters the whole transfer.
                    self.process_dma_transfer(true);
                } else {
                    let block_count = ((self.blksizecnt >> 16) & 0xFFFF) as usize;
                    let start = self.arg as usize * BLOCK_SIZE as usize;
                    self.read_start = start;
                    self.read_cursor = start;
                    self.read_end = start + block_count * BLOCK_SIZE as usize;
                    self.raise(regs::INT_READ_RDY);
                }
            }
            24 | 25 => {
                self.resp[0] = 0;
                if cmdtm & regs::TM_DMA_EN != 0 {
                    // ADMA2 write: the controller masters the whole transfer.
                    self.process_dma_transfer(false);
                } else {
                    let block_count = ((self.blksizecnt >> 16) & 0xFFFF) as usize;
                    let start = self.arg as usize * BLOCK_SIZE as usize;
                    self.write_start = start;
                    self.write_cursor = start;
                    self.write_end = start + block_count * BLOCK_SIZE as usize;
                    self.raise(regs::INT_WRITE_RDY);
                }
            }
            _ => {}
        }
        self.raise(regs::INT_CMD_DONE);
    }
}

impl SdhciHost for MockSdhci {
    fn read32(&mut self, offset: usize) -> Result<u32, DriverError> {
        let value = match offset {
            regs::REG_CONTROL0 => self.control0,
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

    fn await_irq(&mut self) -> CompletionSignal {
        // Model the controller's interrupt firing: reveal any staged
        // completion bits to `INTERRUPT`. In normal mode completions are
        // already visible, so this only counts the park; in the
        // interrupt-delivery model (`healthy_deferred`) the engine cannot
        // make progress without it, proving it parks on the interrupt and
        // never busy-spins. A `silent` controller models a dead interrupt
        // path: the bounded wait elapses with no fire.
        self.await_calls += 1;
        if self.silent {
            return CompletionSignal::TimedOut;
        }
        self.interrupt |= self.staged;
        self.staged = 0;
        CompletionSignal::Fired
    }

    fn dma_region(&mut self) -> Option<DmaRegion<'_>> {
        if !self.dma_capable {
            return None;
        }
        let device_base = self.dma_base;
        Some(DmaRegion {
            bytes: &mut self.dma_buf,
            device_base,
        })
    }

    fn sync_dma_range(&mut self, offset: usize, len: usize) {
        self.dma_syncs.push((offset, len));
    }

    fn write32(&mut self, offset: usize, value: u32) -> Result<(), DriverError> {
        match offset {
            // The host-controller reset bit self-clears once reset is
            // complete.
            regs::REG_CONTROL1 => self.control1 = value & !regs::CONTROL1_SRST_HC,
            // The host-controller reset clears SD Bus Power; the driver
            // re-powers the rail here.
            regs::REG_CONTROL0 => {
                // Model the full register so the driver's read-modify-write
                // (e.g. setting the 4-bit width bit) preserves the power and
                // voltage bits, exactly as it would on metal. A rail that
                // cannot come up (`power_wired == false`) never latches the
                // power bit, leaving the bus dark.
                self.control0 = if self.power_wired {
                    value
                } else {
                    value & !regs::CONTROL0_BUS_POWER
                };
                self.power_on = self.control0 & regs::CONTROL0_BUS_POWER != 0;
            }
            // The interrupt register is write-1-to-clear.
            regs::REG_INTERRUPT => self.interrupt &= !value,
            regs::REG_ARG1 => self.arg = value,
            regs::REG_BLKSIZECNT => self.blksizecnt = value,
            regs::REG_ADMA_ADDR => self.adma_addr = value,
            regs::REG_CMDTM => self.process_command(value),
            regs::REG_DATA => self.accept_data_word(value),
            regs::REG_IRPT_EN if self.assert_irpt_en => {
                assert_eq!(
                    value,
                    regs::INT_SIGNAL_ENABLE,
                    "bring-up must enable the completion + error interrupt sources"
                );
            }
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
fn bring_up_switches_to_the_4bit_bus_and_data_clock() {
    // After identification the engine must leave the card on the 4-bit bus
    // at the data clock, not the 1-bit identification-clock path bring-up
    // selects — the difference between ~50 KB/s and ~6 MB/s. This is the regression for the slow Pi-4 boot where the store
    // scan and every bundle read ran at the identification clock.
    let dev = Emmc2::open(MockSdhci::healthy(7)).expect("identification");

    // The card was told to use the 4-bit bus (ACMD6 arg `0b10`) and the
    // controller's data-width bit is set to match.
    assert_eq!(dev.host().acmd6_arg, Some(command::BUS_WIDTH_4BIT_ARG));
    assert_ne!(
        dev.host().control0 & regs::CONTROL0_DATA_WIDTH_4BIT,
        0,
        "the controller drives the 4-bit bus"
    );

    // The SD clock was reprogrammed from the identification divisor to the
    // (much smaller) data divisor and left enabled.
    let freq_sel = (dev.host().control1 >> regs::CONTROL1_CLK_FREQ_SHIFT) & 0xFF;
    assert_eq!(
        freq_sel, DATA_CLOCK_DIVISOR,
        "the SD clock runs at the data-rate divisor, not the identification one"
    );
    assert_ne!(
        dev.host().control1 & regs::CONTROL1_CLK_EN,
        0,
        "SDCLK is re-enabled after the frequency change"
    );

    // The speed step must not have disturbed the SD bus power the same
    // CONTROL0 register holds.
    assert!(
        dev.host().power_on,
        "the 4-bit width read-modify-write preserved SD bus power"
    );
}

#[test]
fn the_data_clock_stays_within_sd_default_speed() {
    // The data divisor is derived from the identification divisor so the
    // data clock is exactly 32× the (≤400 kHz) identification clock, i.e.
    // ≤12.8 MHz — within SD Default Speed's 25 MHz ceiling, so no high-speed
    // mode switch is needed (: a format-driven
    // bound, derived not guessed).
    assert_eq!(DATA_CLOCK_DIVISOR * 32, IDENT_CLOCK_DIVISOR);
    assert_ne!(DATA_CLOCK_DIVISOR, 0, "the divisor must clock the bus");
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
fn interrupt_driven_read_parks_until_the_controller_signals() {
    // The controller delivers every completion only through its interrupt
    // (`healthy_deferred`): a completion bit is invisible in `INTERRUPT`
    // until the engine parks on `await_irq`. A successful read therefore
    // proves the engine waits on the interrupt for each of CMD_DONE,
    // READ_RDY, and DATA_DONE — it never busy-spins a status register. Under the previous busy-spin code
    // `await_irq` was never called, so the read would spin to the poll
    // budget and fail closed instead of completing.
    let mut mock = MockSdhci::healthy_deferred(7);
    mock.fill_block(5, 0x20);
    let mut dev = Emmc2::open(mock).expect("identification");
    let before = dev.host().await_calls;

    let mut buf = [0u8; BLOCK_SIZE as usize];
    dev.read_blocks(5, &mut buf).expect("read");
    assert_eq!(buf.as_slice(), MockSdhci::expected_block(0x20).as_slice());
    assert!(
        dev.host().await_calls > before,
        "the read must park on the controller interrupt, not busy-spin"
    );
}

#[test]
fn reset_enables_the_completion_interrupt_signal() {
    // Bring-up programs `IRPT_EN` with the completion + error sources so the
    // controller actually raises its CPU line for the events the engine
    // parks on; a zero `IRPT_EN` (the old PIO bring-up)
    // would leave the line dead and every `await_irq` parked forever.
    let mut mock = MockSdhci::healthy(7);
    mock.assert_irpt_en = true;
    let _dev = Emmc2::open(mock).expect("identification");
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
    assert_eq!(
        Emmc2::open(mock).err(),
        Some(BringUpFault {
            stage: BringUpStage::OpCond,
            error: DriverError::Unsupported,
        })
    );
}

#[test]
fn non_v2_card_is_unsupported() {
    let mut mock = MockSdhci::healthy(7);
    // A card that does not echo CMD8's check pattern is pre-v2.
    mock.if_cond_echo = false;
    assert_eq!(
        Emmc2::open(mock).err(),
        Some(BringUpFault {
            stage: BringUpStage::SendIfCond,
            error: DriverError::Unsupported,
        })
    );
}

#[test]
fn csd_v1_card_is_unsupported() {
    let mut mock = MockSdhci::healthy(7);
    // High-capacity OCR but a structure-v1 CSD: rejected by the decoder
    // rather than mis-read.
    mock.csd_structure_v2 = false;
    assert_eq!(
        Emmc2::open(mock).err(),
        Some(BringUpFault {
            stage: BringUpStage::SendCsd,
            error: DriverError::Unsupported,
        })
    );
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
    // A tiny budget keeps the bounded wait quick; it must fail closed. The
    // stall blocks command completion (not the clock bring-up), so the
    // first command — `CMD0 GO_IDLE_STATE` — is where the bounded wait
    // times out.
    assert_eq!(
        Emmc2::open_with_budget(mock, 8).err(),
        Some(BringUpFault {
            stage: BringUpStage::GoIdle,
            error: DriverError::DeviceFault,
        })
    );
}

#[test]
fn unpowered_bus_fails_closed_at_first_command() {
    // The full host-controller reset clears SD Bus Power. If the driver did
    // not re-power the rail, command-complete would never assert and the
    // first command (`CMD0 GO_IDLE_STATE`) would time out — the exact metal
    // symptom (`stage=CMD0 GO_IDLE_STATE`, `plans/PI.md` P8/B4). Modelling a
    // rail that refuses to come up proves the engine depends on the
    // power-on write `reset_and_clock` now performs.
    let mut mock = MockSdhci::healthy(7);
    mock.power_wired = false;
    assert_eq!(
        Emmc2::open_with_budget(mock, 8).err(),
        Some(BringUpFault {
            stage: BringUpStage::GoIdle,
            error: DriverError::DeviceFault,
        })
    );
}

#[test]
fn write_single_block_persists_to_card() {
    let mut dev = Emmc2::open(MockSdhci::healthy(7)).expect("identification");
    let payload = MockSdhci::expected_block(0x40);
    dev.write_blocks(3, &payload).expect("write");

    let mut buf = [0u8; BLOCK_SIZE as usize];
    dev.read_blocks(3, &mut buf).expect("read back");
    assert_eq!(buf.as_slice(), payload.as_slice());
}

#[test]
fn write_multiple_blocks_persist_contiguously() {
    let mut dev = Emmc2::open(MockSdhci::healthy(7)).expect("identification");
    let bs = BLOCK_SIZE as usize;
    let mut payload = MockSdhci::expected_block(0x10);
    payload.extend_from_slice(&MockSdhci::expected_block(0x50));
    payload.extend_from_slice(&MockSdhci::expected_block(0x90));
    dev.write_blocks(1, &payload).expect("write");

    let mut buf = [0u8; 3 * BLOCK_SIZE as usize];
    dev.read_blocks(1, &mut buf).expect("read back");
    assert_eq!(&buf[0..bs], MockSdhci::expected_block(0x10).as_slice());
    assert_eq!(&buf[bs..2 * bs], MockSdhci::expected_block(0x50).as_slice());
    assert_eq!(
        &buf[2 * bs..3 * bs],
        MockSdhci::expected_block(0x90).as_slice()
    );
}

#[test]
fn write_leaves_neighbouring_blocks_untouched() {
    let mut mock = MockSdhci::healthy(7);
    mock.fill_block(2, 0x20);
    mock.fill_block(4, 0x60);
    let mut dev = Emmc2::open(mock).expect("identification");
    dev.write_blocks(3, &MockSdhci::expected_block(0xA0))
        .expect("write");

    let mut buf = [0u8; 3 * BLOCK_SIZE as usize];
    dev.read_blocks(2, &mut buf).expect("read back");
    let bs = BLOCK_SIZE as usize;
    assert_eq!(&buf[0..bs], MockSdhci::expected_block(0x20).as_slice());
    assert_eq!(&buf[bs..2 * bs], MockSdhci::expected_block(0xA0).as_slice());
    assert_eq!(
        &buf[2 * bs..3 * bs],
        MockSdhci::expected_block(0x60).as_slice()
    );
}

#[test]
fn write_rejects_short_and_misaligned_buffers() {
    let mut dev = Emmc2::open(MockSdhci::healthy(7)).expect("identification");
    let empty: [u8; 0] = [];
    assert_eq!(
        dev.write_blocks(0, &empty),
        Err(DriverError::BufferTooSmall)
    );
    let partial = [0u8; 200];
    assert_eq!(
        dev.write_blocks(0, &partial),
        Err(DriverError::BufferTooSmall)
    );
}

#[test]
fn write_rejects_out_of_range_lba() {
    let mut dev = Emmc2::open(MockSdhci::healthy(0)).expect("identification");
    // c_size 0 → 1024 blocks; LBA 1024 is one past the end.
    let payload = [0u8; BLOCK_SIZE as usize];
    assert_eq!(
        dev.write_blocks(1024, &payload),
        Err(DriverError::LengthOutOfRange)
    );
}

#[test]
fn write_command_error_fails_closed() {
    let mut mock = MockSdhci::healthy(7);
    // Inject an error response on the single-block write command.
    mock.error_on_index = Some(24);
    let mut dev = Emmc2::open(mock).expect("identification");
    let payload = [0u8; BLOCK_SIZE as usize];
    assert_eq!(dev.write_blocks(0, &payload), Err(DriverError::DeviceFault));
}

#[test]
fn a_silent_controller_fails_closed_instead_of_hanging() {
    // A controller whose interrupt path is dead: completions are staged
    // (never visible without a fire) and every bounded wait times out. The
    // engine must surface `DeviceFault` on the *first* timed-out wait —
    // never re-poll a silent device through its whole poll budget, and
    // never leave the caller waiting forever.
    let mut mock = MockSdhci::healthy_deferred(7);
    mock.silent = true;
    let Err(fault) = Emmc2::open(mock) else {
        panic!("a silent controller cannot identify")
    };
    assert_eq!(DriverError::from(fault), DriverError::DeviceFault);
}

// --- ADMA2 DMA transfer path ----------------------------------------------

#[test]
fn bring_up_selects_adma2_when_a_dma_region_is_present() {
    // A host that offers a DMA staging region makes bring-up select the
    // ADMA2 fast path: the controller's DMA-select field is programmed to
    // ADMA2, and the read-modify-write preserves SD bus power and the
    // 4-bit width the same CONTROL0 register holds.
    let dev = Emmc2::open(MockSdhci::healthy_dma(7, STORE_BLOCKS)).expect("identification");
    assert_eq!(
        dev.host().control0 & regs::CONTROL0_DMA_SELECT_MASK,
        regs::CONTROL0_DMA_SELECT_ADMA2,
        "DMA-capable bring-up selects ADMA2"
    );
    assert!(dev.host().power_on, "ADMA2 select preserved SD bus power");
    assert_ne!(
        dev.host().control0 & regs::CONTROL0_DATA_WIDTH_4BIT,
        0,
        "ADMA2 select preserved the 4-bit bus width"
    );
}

#[test]
fn a_pio_only_host_stays_on_the_pio_path() {
    // A host that grants no DMA region leaves the controller on programmed
    // I/O: the DMA-select field is never set to ADMA2 (DMA where possible,
    // PIO otherwise).
    let dev = Emmc2::open(MockSdhci::healthy(7)).expect("identification");
    assert_ne!(
        dev.host().control0 & regs::CONTROL0_DMA_SELECT_MASK,
        regs::CONTROL0_DMA_SELECT_ADMA2,
        "a PIO-only host is never switched to ADMA2"
    );
}

#[test]
fn dma_read_single_block_returns_card_data() {
    let mut mock = MockSdhci::healthy_dma(7, STORE_BLOCKS);
    mock.fill_block(3, 0x40);
    let mut dev = Emmc2::open(mock).expect("identification");

    let mut buf = [0u8; BLOCK_SIZE as usize];
    dev.read_blocks(3, &mut buf).expect("dma read");
    assert_eq!(buf.as_slice(), MockSdhci::expected_block(0x40).as_slice());
    assert_eq!(
        dev.host().dma_syncs,
        [
            (0, BLOCK_SIZE as usize),
            (DMA_DESC_OFFSET, adma::DESC_BYTES),
            (0, BLOCK_SIZE as usize),
        ],
        "publish data+descriptor before DMA, then consume read data"
    );
}

#[test]
fn dma_read_multiple_blocks_returns_contiguous_data() {
    let mut mock = MockSdhci::healthy_dma(7, STORE_BLOCKS);
    mock.fill_block(1, 0x10);
    mock.fill_block(2, 0x50);
    mock.fill_block(3, 0x90);
    let mut dev = Emmc2::open(mock).expect("identification");

    let mut buf = [0u8; 3 * BLOCK_SIZE as usize];
    dev.read_blocks(1, &mut buf).expect("dma read");
    let bs = BLOCK_SIZE as usize;
    assert_eq!(&buf[0..bs], MockSdhci::expected_block(0x10).as_slice());
    assert_eq!(&buf[bs..2 * bs], MockSdhci::expected_block(0x50).as_slice());
    assert_eq!(
        &buf[2 * bs..3 * bs],
        MockSdhci::expected_block(0x90).as_slice()
    );
}

#[test]
fn dma_write_single_block_persists_to_card() {
    let mut dev = Emmc2::open(MockSdhci::healthy_dma(7, STORE_BLOCKS)).expect("identification");
    let payload = MockSdhci::expected_block(0x40);
    dev.write_blocks(3, &payload).expect("dma write");

    let mut buf = [0u8; BLOCK_SIZE as usize];
    dev.read_blocks(3, &mut buf).expect("dma read back");
    assert_eq!(buf.as_slice(), payload.as_slice());
}

#[test]
fn dma_write_multiple_blocks_persist_contiguously() {
    let mut dev = Emmc2::open(MockSdhci::healthy_dma(7, STORE_BLOCKS)).expect("identification");
    let bs = BLOCK_SIZE as usize;
    let mut payload = MockSdhci::expected_block(0x10);
    payload.extend_from_slice(&MockSdhci::expected_block(0x50));
    payload.extend_from_slice(&MockSdhci::expected_block(0x90));
    dev.write_blocks(1, &payload).expect("dma write");

    let mut buf = [0u8; 3 * BLOCK_SIZE as usize];
    dev.read_blocks(1, &mut buf).expect("dma read back");
    assert_eq!(&buf[0..bs], MockSdhci::expected_block(0x10).as_slice());
    assert_eq!(&buf[bs..2 * bs], MockSdhci::expected_block(0x50).as_slice());
    assert_eq!(
        &buf[2 * bs..3 * bs],
        MockSdhci::expected_block(0x90).as_slice()
    );
}

#[test]
fn dma_write_leaves_neighbouring_blocks_untouched() {
    let mut mock = MockSdhci::healthy_dma(7, STORE_BLOCKS);
    mock.fill_block(2, 0x20);
    mock.fill_block(4, 0x60);
    let mut dev = Emmc2::open(mock).expect("identification");
    dev.write_blocks(3, &MockSdhci::expected_block(0xA0))
        .expect("dma write");

    let mut buf = [0u8; 3 * BLOCK_SIZE as usize];
    dev.read_blocks(2, &mut buf).expect("dma read back");
    let bs = BLOCK_SIZE as usize;
    assert_eq!(&buf[0..bs], MockSdhci::expected_block(0x20).as_slice());
    assert_eq!(&buf[bs..2 * bs], MockSdhci::expected_block(0xA0).as_slice());
    assert_eq!(
        &buf[2 * bs..3 * bs],
        MockSdhci::expected_block(0x60).as_slice()
    );
}

#[test]
fn dma_transfer_larger_than_one_chunk_is_split_and_reassembled() {
    // A transfer of more than DMA_STAGE_BLOCKS blocks must be split into
    // successive DMA commands (each a staging-window chunk) and reassembled
    // in order — the chunk loop's LBA/offset arithmetic. Read a span that
    // spills into a second chunk and check every block against its pattern.
    let total = DMA_STAGE_BLOCKS + 40;
    let store_blocks = total + 8;
    // c_size 0 → 1024-block geometry, larger than the span read below.
    let mut mock = MockSdhci::healthy_dma(0, store_blocks);
    mock.fill_blocks(0, total, 0x01);
    let mut dev = Emmc2::open(mock).expect("identification");

    let bs = BLOCK_SIZE as usize;
    let mut buf = vec![0u8; total * bs];
    dev.read_blocks(0, &mut buf).expect("dma read");
    for n in 0..total {
        assert_eq!(
            &buf[n * bs..(n + 1) * bs],
            MockSdhci::expected_block(0x01u8.wrapping_add(u8::try_from(n % 256).unwrap_or(0)))
                .as_slice(),
            "block {n} reassembled from its chunk",
        );
    }
    let tail_bytes = 40 * bs;
    assert_eq!(
        dev.host().dma_syncs,
        [
            (0, DMA_DATA_BYTES),
            (DMA_DESC_OFFSET, adma::DESC_BYTES),
            (0, DMA_DATA_BYTES),
            (0, tail_bytes),
            (DMA_DESC_OFFSET, adma::DESC_BYTES),
            (0, tail_bytes),
        ],
        "each chunk has publication and read-consumption synchronization"
    );
}

#[test]
fn dma_write_then_read_round_trips_across_chunks() {
    // Write a multi-chunk payload by DMA and read it back by DMA: proves
    // the write chunk loop stages each chunk from the right buffer offset
    // and the round trip is lossless across the chunk boundary.
    let total = DMA_STAGE_BLOCKS + 5;
    let store_blocks = total + 8;
    let mut dev = Emmc2::open(MockSdhci::healthy_dma(0, store_blocks)).expect("identification");
    let bs = BLOCK_SIZE as usize;
    let mut payload = vec![0u8; total * bs];
    for (n, block) in payload.chunks_mut(bs).enumerate() {
        block.copy_from_slice(
            MockSdhci::expected_block(0x80u8.wrapping_add(u8::try_from(n % 256).unwrap_or(0)))
                .as_slice(),
        );
    }
    dev.write_blocks(0, &payload).expect("dma write");

    let mut buf = vec![0u8; total * bs];
    dev.read_blocks(0, &mut buf).expect("dma read back");
    assert_eq!(buf, payload, "multi-chunk DMA round trip is lossless");
}

#[test]
fn dma_read_command_error_fails_closed() {
    let mut mock = MockSdhci::healthy_dma(7, STORE_BLOCKS);
    // Inject an error response on the single-block read command.
    mock.error_on_index = Some(17);
    let mut dev = Emmc2::open(mock).expect("identification");
    let mut buf = [0u8; BLOCK_SIZE as usize];
    assert_eq!(dev.read_blocks(0, &mut buf), Err(DriverError::DeviceFault));
}

#[test]
fn dma_read_rejects_out_of_range_lba() {
    // c_size 0 → 1024 blocks; validation happens before any DMA staging.
    let mut dev = Emmc2::open(MockSdhci::healthy_dma(0, STORE_BLOCKS)).expect("identification");
    let mut buf = [0u8; BLOCK_SIZE as usize];
    assert_eq!(
        dev.read_blocks(1024, &mut buf),
        Err(DriverError::LengthOutOfRange)
    );
}

// --- `wiring` capability gate ---------------------------------------------

/// A no-op [`CompletionWait`] for the `wiring` capability tests: those
/// return at the capability/mapper gate before the engine ever parks, so
/// the waiter is never driven.
struct NoIrq;
impl crate::CompletionWait for NoIrq {
    fn await_irq(&self) -> CompletionSignal {
        CompletionSignal::Fired
    }
}

/// Minimal RAM-backed mapper for the `wiring` capability tests. The full
/// register chain is proven through [`MockSdhci`]; this only exercises the
/// capability / mapper gate.
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
        wiring::open_discovered(&host, EMMC2_PHYS, NoIrq).err(),
        Some(BringUpFault {
            stage: BringUpStage::MapWindow,
            error: DriverError::PermissionDenied,
        })
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
        wiring::open_discovered(&host, EMMC2_PHYS, NoIrq).err(),
        Some(BringUpFault {
            stage: BringUpStage::MapWindow,
            error: DriverError::Unsupported,
        })
    );
}

#[test]
fn bind_table_matches_the_bcm2711_emmc2_node() {
    use rustos_abi::HwMatchKey;

    // One entry at the declared exact-match priority, matching a
    // discovered node carrying the EMMC2 `compatible` string the
    // aarch64 `FdtDiscovery` emits.
    assert_eq!(BIND_KEYS.len(), 1);
    assert_eq!(BIND_KEYS[0].priority, BIND_PRIORITY);
    let emmc2 = HwMatchKey::compatible(b"brcm,bcm2711-emmc2").expect("fits");
    assert!(BIND_KEYS[0].key.matches(&emmc2));

    // The sibling BCM2711 PCIe root-complex node and an unrelated node
    // both fail the match — the caller leaves them unbound rather than
    // guessing.
    let pcie = HwMatchKey::compatible(b"brcm,bcm2711-pcie").expect("fits");
    assert!(!BIND_KEYS[0].key.matches(&pcie));
}

#[test]
fn bring_up_stage_names_every_step_distinctly() {
    use alloc::collections::BTreeSet;

    // Every stage maps to a non-empty, unique operator-facing name (the
    // diagnostic contract the metal `stage=` log field carries). A missing
    // or duplicated name would mislead the operator about which SD command
    // stalled.
    let stages = [
        BringUpStage::MapWindow,
        BringUpStage::ResetClock,
        BringUpStage::GoIdle,
        BringUpStage::SendIfCond,
        BringUpStage::OpCond,
        BringUpStage::AllSendCid,
        BringUpStage::SendRelativeAddr,
        BringUpStage::SendCsd,
        BringUpStage::SelectCard,
        BringUpStage::SetBlockLen,
        BringUpStage::SetBusWidth,
        BringUpStage::RaiseClock,
    ];
    let names: BTreeSet<&'static str> = stages.iter().map(|s| s.as_str()).collect();
    assert_eq!(names.len(), stages.len());
    assert!(names.iter().all(|n| !n.is_empty()));
}

#[test]
fn bring_up_fault_converts_to_its_driver_error() {
    // A consumer that only needs the driver-ABI `DriverError` drops the
    // stage with `?` / `DriverError::from`.
    let fault = BringUpFault {
        stage: BringUpStage::OpCond,
        error: DriverError::DeviceFault,
    };
    assert_eq!(DriverError::from(fault), DriverError::DeviceFault);
}
