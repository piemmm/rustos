//! Unit tests for the xHCI protocol layers against a register-level
//! mock controller plus an in-memory ring/DMA model (mirrors the
//! `emmc2` `MockSdhci` seam).

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use super::device::{
    DeviceDescriptor, DmaRegion, UsbDevice, PRIMED_REPORTS, REPORT_LEN, RING_TRBS,
};
use super::ring::{EventRingCursor, ProducerRing};
use super::trb::{CompletionCode, Trb, TrbType, CONTROL_CYCLE, TRB_LEN};
use super::*;
use rustos_abi::driver::input::{Input, ReportSource};
use rustos_abi::driver::DriverKind;
use rustos_drv_input_usb_hid::BootKeyboard;

/// The mock's `CAPLENGTH` (so its operational base).
const MOCK_CAPLENGTH: u32 = 0x20;
/// The mock's doorbell-array offset.
const MOCK_DBOFF: u32 = 0x1000;
/// The mock's runtime-block offset.
const MOCK_RTSOFF: u32 = 0x2000;
/// The mock's register-window byte length.
const MOCK_WINDOW_LEN: usize = 0x3000;
/// Device-visible base address of the shared DMA buffer.
const MOCK_DMA_BASE: u64 = 0x0010_0000;
/// Byte length of the shared DMA buffer (the layout for 32 slots with
/// 64-byte contexts needs ~5.1 KiB).
const MOCK_DMA_LEN: usize = 0x2000;
/// The mock's 64-byte contexts (its `HCCPARAMS1` sets CSZ).
const MOCK_CTX_SIZE: usize = 64;

/// Memory shared between the engine's [`DmaRegion`] and the mock
/// controller's device model — the in-memory stand-in for DMA.
type SharedMem = Rc<RefCell<Vec<u8>>>;

fn shared_mem() -> SharedMem {
    Rc::new(RefCell::new(alloc::vec![0u8; MOCK_DMA_LEN]))
}

/// The engine-side view of the shared buffer.
struct MockDma {
    mem: SharedMem,
    phys: u64,
}

impl DmaRegion for MockDma {
    fn phys(&self) -> u64 {
        self.phys
    }

    fn len(&self) -> usize {
        self.mem.borrow().len()
    }

    fn read(&mut self, offset: usize, buf: &mut [u8]) -> Result<(), DriverError> {
        let mem = self.mem.borrow();
        let end = offset
            .checked_add(buf.len())
            .ok_or(DriverError::OutOfRange)?;
        if end > mem.len() {
            return Err(DriverError::OutOfRange);
        }
        buf.copy_from_slice(&mem[offset..end]);
        Ok(())
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<(), DriverError> {
        let mut mem = self.mem.borrow_mut();
        let end = offset
            .checked_add(bytes.len())
            .ok_or(DriverError::OutOfRange)?;
        if end > mem.len() {
            return Err(DriverError::OutOfRange);
        }
        mem[offset..end].copy_from_slice(bytes);
        Ok(())
    }
}

/// The 18-byte device descriptor fixture the model answers
/// `GET_DESCRIPTOR(device)` with (a generic boot keyboard).
const MOCK_DESCRIPTOR: [u8; 18] = [
    18, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x6D, 0x04, 0x77, 0xC0, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x01,
];

/// Register-level xHCI model: the capability block, `USBCMD`/`USBSTS`
/// halt/reset behaviour, four `PORTSC` ports, a doorbell write log,
/// and — when a shared DMA buffer is attached — an in-memory device
/// model that consumes the command/transfer rings and produces events
/// exactly as a controller with one attached HID device would.
///
/// The booleans mirror independent hardware bits and fault-injection
/// knobs, not a state machine — the `struct_excessive_bools` lint is
/// allowed here for the same reason as the `emmc2` `MockSdhci`.
#[allow(clippy::struct_excessive_bools)]
struct MockXhci {
    cap_dword0: u32,
    hcsparams1: u32,
    hccparams1: u32,
    dboff: u32,
    rtsoff: u32,
    usbcmd: u32,
    portsc: [u32; 4],
    /// `USBSTS` reads report Controller Not Ready until this many
    /// status reads have happened.
    cnr_reads: u32,
    /// `USBCMD` reads keep `HCRST` set for this many reads after a
    /// reset is requested (models the self-clearing bit).
    hcrst_reads: u32,
    /// When set, `HCRST` never self-clears (a stuck controller).
    hcrst_stuck: bool,
    /// When set, `USBSTS` reports Controller Not Ready forever.
    cnr_stuck: bool,
    doorbells: Vec<(usize, u32)>,
    /// `PORTSC` reads report Port Reset in progress for this many
    /// reads after a reset write (models the self-clearing bit).
    port_reset_reads: u32,
    /// The port index a reset is in progress on.
    port_reset_port: usize,
    /// The shared DMA buffer, when the device model is attached.
    mem: Option<SharedMem>,
    // Captured DMA-programming registers.
    config: u32,
    dcbaap: [u32; 2],
    crcr: [u32; 2],
    erstsz: u32,
    erstba: [u32; 2],
    erdp: [u32; 2],
    // Device-model ring consumer / event producer state.
    cmd_index: usize,
    cmd_cycle: bool,
    ep0_base: u64,
    ep0_index: usize,
    ep0_cycle: bool,
    int_base: u64,
    int_index: usize,
    int_cycle: bool,
    event_index: usize,
    event_cycle: bool,
    // Device-model device state.
    next_slot: u8,
    active_slot: u8,
    addressed: bool,
    configured: bool,
    configuration: Option<u8>,
    protocol: Option<u8>,
    pending_setup: Option<[u8; 8]>,
    /// Pending IN data stage: TRB address, buffer, length, ISP.
    pending_data: Option<(u64, u64, u32, bool)>,
    pending_reports: VecDeque<Vec<u8>>,
    /// When set, class requests (`SET_PROTOCOL`) answer STALL.
    stall_class_requests: bool,
    /// When set, report completions forge a residual above the TRB
    /// length (a hostile controller claim).
    forge_report_residual: bool,
}

impl MockXhci {
    fn new() -> Self {
        Self {
            cap_dword0: 0x0110_0000 | MOCK_CAPLENGTH, // xHCI 1.1
            hcsparams1: 0x0400_0020,                  // 4 ports, 32 slots
            hccparams1: 0x0000_0005,                  // AC64 + CSZ
            dboff: MOCK_DBOFF,
            rtsoff: MOCK_RTSOFF,
            usbcmd: 0,
            portsc: [0; 4],
            cnr_reads: 0,
            hcrst_reads: 0,
            hcrst_stuck: false,
            cnr_stuck: false,
            doorbells: Vec::new(),
            port_reset_reads: 0,
            port_reset_port: 0,
            mem: None,
            config: 0,
            dcbaap: [0; 2],
            crcr: [0; 2],
            erstsz: 0,
            erstba: [0; 2],
            erdp: [0; 2],
            cmd_index: 0,
            cmd_cycle: true,
            ep0_base: 0,
            ep0_index: 0,
            ep0_cycle: true,
            int_base: 0,
            int_index: 0,
            int_cycle: true,
            event_index: 0,
            event_cycle: true,
            next_slot: 1,
            active_slot: 0,
            addressed: false,
            configured: false,
            configuration: None,
            protocol: None,
            pending_setup: None,
            pending_data: None,
            pending_reports: VecDeque::new(),
            stall_class_requests: false,
            forge_report_residual: false,
        }
    }

    /// A mock with the device model attached and a high-speed HID
    /// device connected and enabled on root-hub port 1.
    fn with_device(mem: &SharedMem) -> Self {
        let mut mock = Self::new();
        mock.mem = Some(Rc::clone(mem));
        mock.portsc[0] =
            regs::PORTSC_CCS | regs::PORTSC_PED | regs::PORTSC_PP | (3 << regs::PORTSC_SPEED_SHIFT);
        mock
    }

    fn op(offset: usize) -> usize {
        MOCK_CAPLENGTH as usize + offset
    }

    fn ir0(offset: usize) -> usize {
        MOCK_RTSOFF as usize + regs::IR0_BASE + offset
    }

    fn qword(pair: [u32; 2]) -> u64 {
        (u64::from(pair[1]) << 32) | u64::from(pair[0])
    }

    // ---- in-memory device model -------------------------------------

    fn mem_offset(addr: u64) -> usize {
        usize::try_from(addr - MOCK_DMA_BASE).expect("device address inside the shared buffer")
    }

    fn read_trb_at(&self, addr: u64) -> Trb {
        let mem = self.mem.as_ref().expect("device model attached").borrow();
        let off = Self::mem_offset(addr);
        let mut image = [0u8; TRB_LEN];
        image.copy_from_slice(&mem[off..off + TRB_LEN]);
        Trb::from_bytes(image)
    }

    fn write_mem(&self, addr: u64, bytes: &[u8]) {
        let mut mem = self
            .mem
            .as_ref()
            .expect("device model attached")
            .borrow_mut();
        let off = Self::mem_offset(addr);
        mem[off..off + bytes.len()].copy_from_slice(bytes);
    }

    fn read_dwords(&self, addr: u64, count: usize) -> Vec<u32> {
        let mem = self.mem.as_ref().expect("device model attached").borrow();
        let off = Self::mem_offset(addr);
        (0..count)
            .map(|i| {
                u32::from_le_bytes([
                    mem[off + i * 4],
                    mem[off + i * 4 + 1],
                    mem[off + i * 4 + 2],
                    mem[off + i * 4 + 3],
                ])
            })
            .collect()
    }

    /// Produce one event TRB into the event segment named by the ERST.
    fn post_event(&mut self, mut event: Trb) {
        let erst = Self::qword(self.erstba);
        let entry = self.read_dwords(erst, 4);
        let segment = (u64::from(entry[1]) << 32) | u64::from(entry[0]);
        let len = usize::try_from(entry[2]).expect("segment length");
        event.control &= !CONTROL_CYCLE;
        if self.event_cycle {
            event.control |= CONTROL_CYCLE;
        }
        self.write_mem(
            segment + (self.event_index * TRB_LEN) as u64,
            &event.to_bytes(),
        );
        self.event_index += 1;
        if self.event_index == len {
            self.event_index = 0;
            self.event_cycle = !self.event_cycle;
        }
    }

    fn post_command_completion(&mut self, command_addr: u64, code: CompletionCode, slot: u8) {
        self.post_event(Trb {
            parameter: command_addr,
            status: u32::from(code.as_u8()) << 24,
            control: (u32::from(TrbType::CommandCompletion.as_u8()) << 10)
                | trb::control_slot(slot),
        });
    }

    fn post_transfer_event(&mut self, trb_addr: u64, code: CompletionCode, dci: u8, residual: u32) {
        self.post_event(Trb {
            parameter: trb_addr,
            status: (u32::from(code.as_u8()) << 24) | residual,
            control: (u32::from(TrbType::TransferEvent.as_u8()) << 10)
                | (u32::from(dci) << 16)
                | trb::control_slot(self.active_slot),
        });
    }

    /// Walk one producer ring from `(index, cycle)`, returning the next
    /// owned TRB and its address, following (and re-cycling over) the
    /// wrap Link TRB exactly as a consumer would (§4.9.2).
    fn next_owned(&self, base: u64, index: &mut usize, cycle: &mut bool) -> Option<(u64, Trb)> {
        loop {
            let addr = base + (*index * TRB_LEN) as u64;
            let trb = self.read_trb_at(addr);
            if trb.cycle() != *cycle {
                return None;
            }
            if trb.trb_type() == Ok(TrbType::Link) {
                if trb.control & trb::CONTROL_LINK_TOGGLE != 0 {
                    *cycle = !*cycle;
                }
                *index = 0;
                continue;
            }
            *index += 1;
            return Some((addr, trb));
        }
    }

    fn process_command_ring(&mut self) {
        let base = Self::qword(self.crcr) & !0x3F;
        loop {
            let (mut index, mut cycle) = (self.cmd_index, self.cmd_cycle);
            let Some((addr, trb)) = self.next_owned(base, &mut index, &mut cycle) else {
                return;
            };
            self.cmd_index = index;
            self.cmd_cycle = cycle;
            match trb.trb_type() {
                Ok(TrbType::EnableSlot) => {
                    let slot = self.next_slot;
                    self.next_slot += 1;
                    self.active_slot = slot;
                    self.post_command_completion(addr, CompletionCode::Success, slot);
                }
                Ok(TrbType::AddressDevice) => {
                    let code = self.handle_address_device(trb.parameter);
                    self.post_command_completion(addr, code, trb.slot_id());
                }
                Ok(TrbType::ConfigureEndpoint) => {
                    let code = self.handle_configure_endpoint(trb.parameter);
                    self.post_command_completion(addr, code, trb.slot_id());
                }
                Ok(TrbType::NoOpCommand) => {
                    self.post_command_completion(addr, CompletionCode::Success, 0);
                }
                _ => {
                    self.post_command_completion(addr, CompletionCode::TrbError, 0);
                }
            }
        }
    }

    /// Read a transfer-ring dequeue pointer out of the endpoint context
    /// at `ctx_addr` (dwords 2/3, DCS masked off).
    fn ep_ctx_dequeue(&self, ctx_addr: u64) -> u64 {
        let dwords = self.read_dwords(ctx_addr, 4);
        ((u64::from(dwords[3]) << 32) | u64::from(dwords[2])) & !0xF
    }

    fn handle_address_device(&mut self, input_ctx: u64) -> CompletionCode {
        let control = self.read_dwords(input_ctx, 2);
        // Add flags must name the slot context and EP0 (A0 | A1).
        if control[1] & 0b11 != 0b11 {
            return CompletionCode::TrbError;
        }
        self.ep0_base = self.ep_ctx_dequeue(input_ctx + 2 * MOCK_CTX_SIZE as u64);
        self.ep0_index = 0;
        self.ep0_cycle = true;
        self.addressed = true;
        CompletionCode::Success
    }

    fn handle_configure_endpoint(&mut self, input_ctx: u64) -> CompletionCode {
        let control = self.read_dwords(input_ctx, 2);
        // Add flags must name the slot context and the interrupt-IN
        // endpoint (A0 | A3).
        if control[1] & 0b1001 != 0b1001 {
            return CompletionCode::TrbError;
        }
        self.int_base = self.ep_ctx_dequeue(input_ctx + 4 * MOCK_CTX_SIZE as u64);
        self.int_index = 0;
        self.int_cycle = true;
        self.configured = true;
        CompletionCode::Success
    }

    fn process_ep0_ring(&mut self) {
        loop {
            let (mut index, mut cycle) = (self.ep0_index, self.ep0_cycle);
            let base = self.ep0_base;
            let Some((addr, trb)) = self.next_owned(base, &mut index, &mut cycle) else {
                return;
            };
            self.ep0_index = index;
            self.ep0_cycle = cycle;
            match trb.trb_type() {
                Ok(TrbType::SetupStage) => {
                    self.pending_setup = Some(trb.parameter.to_le_bytes());
                }
                Ok(TrbType::DataStage) => {
                    self.pending_data = Some((
                        addr,
                        trb.parameter,
                        trb.status & 0x1_FFFF,
                        trb.control & trb::CONTROL_ISP != 0,
                    ));
                }
                Ok(TrbType::StatusStage) => self.execute_control(addr),
                _ => self.post_transfer_event(addr, CompletionCode::TrbError, 1, 0),
            }
        }
    }

    /// Execute the assembled control TD, posting its transfer events.
    fn execute_control(&mut self, status_addr: u64) {
        let Some(setup) = self.pending_setup.take() else {
            self.post_transfer_event(status_addr, CompletionCode::TrbError, 1, 0);
            return;
        };
        let data = self.pending_data.take();
        match (setup[0], setup[1]) {
            // GET_DESCRIPTOR(device)
            (0x80, 0x06) if setup[3] == 0x01 => {
                let Some((data_addr, buffer, len, isp)) = data else {
                    self.post_transfer_event(status_addr, CompletionCode::TrbError, 1, 0);
                    return;
                };
                let requested = usize::min(
                    len as usize,
                    usize::from(u16::from_le_bytes([setup[6], setup[7]])),
                );
                let supplied = usize::min(requested, MOCK_DESCRIPTOR.len());
                self.write_mem(buffer, &MOCK_DESCRIPTOR[..supplied]);
                let residual = len - u32::try_from(supplied).expect("descriptor fits");
                if residual > 0 && isp {
                    self.post_transfer_event(data_addr, CompletionCode::ShortPacket, 1, residual);
                }
            }
            // SET_CONFIGURATION
            (0x00, 0x09) => self.configuration = Some(setup[2]),
            // SET_PROTOCOL (HID class)
            (0x21, 0x0B) => {
                if self.stall_class_requests {
                    self.post_transfer_event(status_addr, CompletionCode::StallError, 1, 0);
                    return;
                }
                self.protocol = Some(setup[2]);
            }
            _ => {
                self.post_transfer_event(status_addr, CompletionCode::StallError, 1, 0);
                return;
            }
        }
        self.post_transfer_event(status_addr, CompletionCode::Success, 1, 0);
    }

    fn process_int_ring(&mut self) {
        while let Some(report) = self.pending_reports.front().cloned() {
            let (mut index, mut cycle) = (self.int_index, self.int_cycle);
            let base = self.int_base;
            let Some((addr, trb)) = self.next_owned(base, &mut index, &mut cycle) else {
                return;
            };
            if trb.trb_type() != Ok(TrbType::Normal) {
                return;
            }
            self.int_index = index;
            self.int_cycle = cycle;
            self.pending_reports.pop_front();
            self.write_mem(trb.parameter, &report);
            let residual = if self.forge_report_residual {
                trb.status + 1
            } else {
                trb.status - u32::try_from(report.len()).expect("report fits")
            };
            let code = if residual > 0 {
                CompletionCode::ShortPacket
            } else {
                CompletionCode::Success
            };
            self.post_transfer_event(addr, code, 3, residual);
        }
    }
}

impl XhciHost for MockXhci {
    fn read32(&mut self, offset: usize) -> Result<u32, DriverError> {
        if offset >= MOCK_WINDOW_LEN {
            return Err(DriverError::DeviceFault);
        }
        if offset == regs::CAPLENGTH_HCIVERSION {
            return Ok(self.cap_dword0);
        }
        if offset == regs::HCSPARAMS1 {
            return Ok(self.hcsparams1);
        }
        if offset == regs::HCCPARAMS1 {
            return Ok(self.hccparams1);
        }
        if offset == regs::DBOFF {
            return Ok(self.dboff);
        }
        if offset == regs::RTSOFF {
            return Ok(self.rtsoff);
        }
        if offset == Self::op(regs::USBCMD) {
            if self.hcrst_reads > 0 && !self.hcrst_stuck {
                self.hcrst_reads -= 1;
                if self.hcrst_reads == 0 {
                    self.usbcmd &= !regs::USBCMD_HCRST;
                }
            }
            return Ok(self.usbcmd);
        }
        if offset == Self::op(regs::USBSTS) {
            let mut status = 0;
            if self.cnr_stuck || self.cnr_reads > 0 {
                self.cnr_reads = self.cnr_reads.saturating_sub(1);
                status |= regs::USBSTS_CNR;
            }
            if self.usbcmd & regs::USBCMD_RUN == 0 {
                status |= regs::USBSTS_HCH;
            }
            return Ok(status);
        }
        if offset == Self::op(regs::CONFIG) {
            return Ok(self.config);
        }
        if offset == Self::ir0(regs::IR_ERSTSZ) {
            return Ok(self.erstsz);
        }
        let portsc_base = Self::op(regs::PORTSC_BASE);
        for port in 0..self.portsc.len() {
            if offset == portsc_base + port * regs::PORTSC_STRIDE {
                if port == self.port_reset_port && self.port_reset_reads > 0 {
                    self.port_reset_reads -= 1;
                    if self.port_reset_reads == 0 {
                        self.portsc[port] &= !regs::PORTSC_PR;
                    }
                }
                return Ok(self.portsc[port]);
            }
        }
        Ok(0)
    }

    fn write32(&mut self, offset: usize, value: u32) -> Result<(), DriverError> {
        if offset >= MOCK_WINDOW_LEN {
            return Err(DriverError::DeviceFault);
        }
        if offset == Self::op(regs::USBCMD) {
            self.usbcmd = value;
            if value & regs::USBCMD_HCRST != 0 {
                // A real reset clears the operational state and the
                // self-clearing bit a few reads later.
                self.hcrst_reads = 3;
            }
            return Ok(());
        }
        if offset == Self::op(regs::CONFIG) {
            self.config = value;
            return Ok(());
        }
        if offset == Self::op(regs::DCBAAP) {
            self.dcbaap[0] = value;
            return Ok(());
        }
        if offset == Self::op(regs::DCBAAP) + 4 {
            self.dcbaap[1] = value;
            return Ok(());
        }
        if offset == Self::op(regs::CRCR) {
            self.crcr[0] = value;
            return Ok(());
        }
        if offset == Self::op(regs::CRCR) + 4 {
            self.crcr[1] = value;
            return Ok(());
        }
        if offset == Self::ir0(regs::IR_ERSTSZ) {
            self.erstsz = value;
            return Ok(());
        }
        if offset == Self::ir0(regs::IR_ERSTBA) {
            self.erstba[0] = value;
            return Ok(());
        }
        if offset == Self::ir0(regs::IR_ERSTBA) + 4 {
            self.erstba[1] = value;
            return Ok(());
        }
        if offset == Self::ir0(regs::IR_ERDP) {
            self.erdp[0] = value;
            return Ok(());
        }
        if offset == Self::ir0(regs::IR_ERDP) + 4 {
            self.erdp[1] = value;
            return Ok(());
        }
        let portsc_base = Self::op(regs::PORTSC_BASE);
        for port in 0..self.portsc.len() {
            if offset == portsc_base + port * regs::PORTSC_STRIDE {
                if value & regs::PORTSC_PR != 0 {
                    // A reset re-enables a connected port; PR reads as
                    // in-progress for a couple of polls.
                    self.portsc[port] |= regs::PORTSC_PED | regs::PORTSC_PR;
                    self.port_reset_reads = 2;
                    self.port_reset_port = port;
                }
                return Ok(());
            }
        }
        let db_base = MOCK_DBOFF as usize;
        if offset >= db_base && offset < db_base + 256 * 4 {
            self.doorbells.push((offset - db_base, value));
            if self.mem.is_some() && self.usbcmd & regs::USBCMD_RUN != 0 {
                let index = (offset - db_base) / 4;
                match (index, value) {
                    (0, 0) => self.process_command_ring(),
                    (_, 1) => self.process_ep0_ring(),
                    (_, 3) => self.process_int_ring(),
                    _ => {}
                }
            }
            return Ok(());
        }
        Ok(())
    }
}

/// Mock driver host modelling the load-time `CAP_DRV_LOAD` grant.
struct MockHost {
    drv_load: bool,
}

impl DriverHost for MockHost {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        match cap {
            CapabilityId::DRV_LOAD => self.drv_load,
            _ => false,
        }
    }
    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }
}

#[test]
fn register_requires_drv_load() {
    assert!(register(&MockHost { drv_load: true }).is_ok());
    assert_eq!(
        register(&MockHost { drv_load: false }),
        Err(DriverError::PermissionDenied)
    );
}

#[test]
fn open_parses_capability_block() {
    let xhci = Xhci::open(MockXhci::new()).expect("bring-up succeeds");
    assert_eq!(xhci.hci_version(), 0x0110);
    assert_eq!(xhci.max_slots(), 32);
    assert_eq!(xhci.max_ports(), 4);
    assert!(xhci.ac64());
    assert!(xhci.csz());
    assert_eq!(xhci.runtime_base(), MOCK_RTSOFF as usize);
}

#[test]
fn open_waits_for_controller_ready() {
    let mut mock = MockXhci::new();
    mock.cnr_reads = 5;
    assert!(Xhci::open(mock).is_ok());
}

#[test]
fn open_halts_a_running_controller_and_resets() {
    let mut mock = MockXhci::new();
    mock.usbcmd = regs::USBCMD_RUN;
    let xhci = Xhci::open(mock).expect("bring-up succeeds");
    // After open the controller was reset: Run/Stop and HCRST clear.
    let mut xhci = xhci;
    let usbcmd = xhci.host.read32(MockXhci::op(regs::USBCMD)).unwrap();
    assert_eq!(usbcmd & (regs::USBCMD_RUN | regs::USBCMD_HCRST), 0);
}

#[test]
fn open_rejects_absent_controller() {
    // An unmapped/absent device reads all-ones.
    let mut mock = MockXhci::new();
    mock.cap_dword0 = u32::MAX;
    assert_eq!(Xhci::open(mock).err(), Some(DriverError::DeviceFault));
}

#[test]
fn open_rejects_implausible_capability_block() {
    let mut mock = MockXhci::new();
    mock.cap_dword0 = 0x0110_0000 | 0x10; // CAPLENGTH below minimum
    assert_eq!(Xhci::open(mock).err(), Some(DriverError::DeviceFault));

    let mut mock = MockXhci::new();
    mock.cap_dword0 = 0x0080_0000 | MOCK_CAPLENGTH; // pre-0.90 version
    assert_eq!(Xhci::open(mock).err(), Some(DriverError::DeviceFault));

    let mut mock = MockXhci::new();
    mock.hcsparams1 = 0x0400_0000; // zero MaxSlots
    assert_eq!(Xhci::open(mock).err(), Some(DriverError::DeviceFault));

    let mut mock = MockXhci::new();
    mock.hcsparams1 = 0x0000_0020; // zero MaxPorts
    assert_eq!(Xhci::open(mock).err(), Some(DriverError::DeviceFault));

    let mut mock = MockXhci::new();
    mock.dboff = 0;
    assert_eq!(Xhci::open(mock).err(), Some(DriverError::DeviceFault));

    let mut mock = MockXhci::new();
    mock.rtsoff = 0;
    assert_eq!(Xhci::open(mock).err(), Some(DriverError::DeviceFault));
}

#[test]
fn open_fails_closed_when_never_ready() {
    let mut mock = MockXhci::new();
    mock.cnr_stuck = true;
    assert_eq!(
        Xhci::open_with_budget(mock, 16).err(),
        Some(DriverError::DeviceFault)
    );
}

#[test]
fn open_fails_closed_when_reset_sticks() {
    let mut mock = MockXhci::new();
    mock.hcrst_stuck = true;
    assert_eq!(
        Xhci::open_with_budget(mock, 16).err(),
        Some(DriverError::DeviceFault)
    );
}

#[test]
fn port_status_decodes_portsc() {
    let mut mock = MockXhci::new();
    // Port 2: connected, enabled, powered, high speed (3), CSC.
    mock.portsc[1] = regs::PORTSC_CCS
        | regs::PORTSC_PED
        | regs::PORTSC_PP
        | regs::PORTSC_CSC
        | (3 << regs::PORTSC_SPEED_SHIFT);
    let mut xhci = Xhci::open(mock).expect("bring-up succeeds");
    let status = xhci.port_status(2).expect("port in range");
    assert!(status.connected());
    assert!(status.enabled());
    assert!(status.powered());
    assert!(status.connect_changed());
    assert!(!status.resetting());
    assert_eq!(status.speed(), 3);
    let empty = xhci.port_status(1).expect("port in range");
    assert!(!empty.connected());
    assert_eq!(empty.speed(), 0);
}

#[test]
fn port_status_rejects_out_of_range_ports() {
    let mut xhci = Xhci::open(MockXhci::new()).expect("bring-up succeeds");
    assert_eq!(xhci.port_status(0), Err(DriverError::OutOfRange));
    assert_eq!(xhci.port_status(5), Err(DriverError::OutOfRange));
}

#[test]
fn doorbells_are_bounds_checked() {
    let mut xhci = Xhci::open(MockXhci::new()).expect("bring-up succeeds");
    xhci.ring_doorbell(0, 0).expect("command doorbell");
    xhci.ring_doorbell(1, 1).expect("device doorbell");
    xhci.ring_doorbell(32, 31).expect("last slot doorbell");
    assert_eq!(xhci.ring_doorbell(33, 1), Err(DriverError::OutOfRange));
    assert_eq!(xhci.ring_doorbell(0, 1), Err(DriverError::OutOfRange));
    assert_eq!(xhci.ring_doorbell(1, 0), Err(DriverError::OutOfRange));
    assert_eq!(xhci.ring_doorbell(1, 32), Err(DriverError::OutOfRange));
    assert_eq!(
        xhci.host.doorbells,
        alloc::vec![(0, 0), (4, 1), (32 * 4, 31)]
    );
}

#[test]
fn trb_type_round_trips_and_fails_closed() {
    for ty in [
        TrbType::Normal,
        TrbType::SetupStage,
        TrbType::DataStage,
        TrbType::StatusStage,
        TrbType::Link,
        TrbType::NoOp,
        TrbType::EnableSlot,
        TrbType::AddressDevice,
        TrbType::ConfigureEndpoint,
        TrbType::NoOpCommand,
        TrbType::TransferEvent,
        TrbType::CommandCompletion,
        TrbType::PortStatusChange,
    ] {
        assert_eq!(TrbType::from_raw(u32::from(ty.as_u8())), Ok(ty));
        assert_eq!(Trb::new(ty, 0, 0, 0).trb_type(), Ok(ty));
    }
    assert_eq!(TrbType::from_raw(0), Err(DriverError::OutOfRange));
    assert_eq!(TrbType::from_raw(63), Err(DriverError::OutOfRange));
}

#[test]
fn event_trb_fields_decode_and_fail_closed() {
    let event = Trb {
        parameter: 0xDEAD_BEEF,
        status: (u32::from(CompletionCode::ShortPacket.as_u8()) << 24) | 5,
        control: (7 << 24) | (u32::from(TrbType::TransferEvent.as_u8()) << 10),
    };
    assert_eq!(event.completion_code(), Ok(CompletionCode::ShortPacket));
    assert_eq!(event.slot_id(), 7);
    let forged = Trb {
        status: 200 << 24,
        ..event
    };
    assert_eq!(forged.completion_code(), Err(DriverError::OutOfRange));
}

/// Apply a [`ring::PushOutcome`](super::ring::PushOutcome) to a local
/// TRB array, standing in for the DMA-memory owner.
fn apply(trbs: &mut [Trb], ring: &ProducerRing, outcome: &super::ring::PushOutcome) {
    trbs[outcome.slot] = outcome.trb;
    if let Some(link) = outcome.link {
        trbs[ring.link_slot()] = link;
    }
}

#[test]
fn producer_ring_rejects_tiny_rings() {
    assert!(matches!(
        ProducerRing::new(2, 0x1000),
        Err(DriverError::LengthOutOfRange)
    ));
}

#[test]
fn producer_ring_stamps_cycle_and_reports_addresses() {
    let mut trbs = [Trb::ZERO; 4];
    let (mut ring, link) = ProducerRing::new(4, 0x1000).expect("ring fits");
    trbs[ring.link_slot()] = link;
    let a = ring.push(Trb::new(TrbType::NoOpCommand, 0, 0, 0)).unwrap();
    apply(&mut trbs, &ring, &a);
    let b = ring.push(Trb::new(TrbType::NoOpCommand, 0, 0, 0)).unwrap();
    apply(&mut trbs, &ring, &b);
    assert_eq!(a.address, 0x1000);
    assert_eq!(b.address, 0x1000 + TRB_LEN as u64);
    assert_eq!(ring.in_flight(), 2);
    // First-pass TRBs carry cycle 1; the link TRB is still unpublished.
    assert!(trbs[0].cycle());
    assert!(trbs[1].cycle());
    assert_eq!(trbs[3].trb_type(), Ok(TrbType::Link));
    assert!(!trbs[3].cycle());
}

#[test]
fn producer_ring_rejects_caller_owned_fields() {
    let (mut ring, _link) = ProducerRing::new(4, 0x1000).expect("ring fits");
    assert!(matches!(
        ring.push(Trb::new(TrbType::NoOpCommand, 0, 0, CONTROL_CYCLE)),
        Err(DriverError::OutOfRange)
    ));
    assert!(matches!(
        ring.push(Trb::new(TrbType::Link, 0x1000, 0, 0)),
        Err(DriverError::OutOfRange)
    ));
}

#[test]
fn producer_ring_full_fails_closed_and_retire_reopens() {
    let (mut ring, _link) = ProducerRing::new(4, 0x1000).expect("ring fits");
    let no_op = Trb::new(TrbType::NoOpCommand, 0, 0, 0);
    ring.push(no_op).expect("slot 0");
    ring.push(no_op).expect("slot 1");
    assert!(matches!(ring.push(no_op), Err(DriverError::Busy)));
    ring.retire_one().expect("one completion");
    ring.push(no_op).expect("freed slot");
    assert_eq!(ring.retire_one(), Ok(()));
    assert_eq!(ring.retire_one(), Ok(()));
    assert_eq!(ring.retire_one(), Err(DriverError::OutOfRange));
}

#[test]
fn producer_ring_wrap_publishes_link_and_toggles_cycle() {
    let mut trbs = [Trb::ZERO; 4];
    let (mut ring, link) = ProducerRing::new(4, 0x1000).expect("ring fits");
    trbs[ring.link_slot()] = link;
    let no_op = Trb::new(TrbType::NoOpCommand, 0, 0, 0);
    let a = ring.push(no_op).expect("slot 0");
    apply(&mut trbs, &ring, &a);
    assert!(a.link.is_none());
    let b = ring.push(no_op).expect("slot 1");
    apply(&mut trbs, &ring, &b);
    ring.retire_one().expect("completion 0");
    ring.retire_one().expect("completion 1");
    // Third push lands in slot 2 — the last data slot — re-publishing
    // the link TRB under cycle 1 and toggling the producer to cycle 0.
    let c = ring.push(no_op).expect("slot 2 wraps");
    apply(&mut trbs, &ring, &c);
    assert_eq!(c.address, 0x1000 + 2 * TRB_LEN as u64);
    assert!(c.link.is_some(), "wrap re-publishes the link TRB");
    // Fourth push lands back in slot 0 under the toggled cycle.
    let d = ring.push(no_op).expect("slot 0 second pass");
    apply(&mut trbs, &ring, &d);
    assert_eq!(d.address, 0x1000);
    assert!(trbs[3].cycle(), "link TRB published under cycle 1");
    assert_eq!(trbs[3].trb_type(), Ok(TrbType::Link));
    assert!(trbs[2].cycle(), "first-pass TRB carries cycle 1");
    assert!(!trbs[0].cycle(), "second-pass TRB carries cycle 0");
}

#[test]
fn event_cursor_rejects_empty_segment() {
    assert!(matches!(
        EventRingCursor::new(0),
        Err(DriverError::LengthOutOfRange)
    ));
}

#[test]
fn event_cursor_consumes_matching_cycle_only() {
    let mut segment = [Trb::ZERO; 3];
    let mut cursor = EventRingCursor::new(3).expect("segment fits");
    // Nothing produced yet: every slot carries cycle 0, cursor wants 1.
    assert_eq!(cursor.pop(&segment), Ok(None));
    segment[0] = Trb::new(
        TrbType::CommandCompletion,
        0x1000,
        u32::from(CompletionCode::Success.as_u8()) << 24,
        CONTROL_CYCLE,
    );
    let event = cursor.pop(&segment).expect("read ok").expect("one event");
    assert_eq!(event.trb_type(), Ok(TrbType::CommandCompletion));
    assert_eq!(cursor.dequeue_index(), 1);
    assert_eq!(cursor.pop(&segment), Ok(None));
}

#[test]
fn event_cursor_wraps_and_toggles_expectation() {
    let mut segment = [Trb::ZERO; 2];
    let mut cursor = EventRingCursor::new(2).expect("segment fits");
    let event = |cycle: bool| Trb {
        parameter: 0,
        status: u32::from(CompletionCode::Success.as_u8()) << 24,
        control: (u32::from(TrbType::PortStatusChange.as_u8()) << 10)
            | if cycle { CONTROL_CYCLE } else { 0 },
    };
    segment[0] = event(true);
    segment[1] = event(true);
    assert!(cursor.pop(&segment).unwrap().is_some());
    assert!(cursor.pop(&segment).unwrap().is_some());
    assert_eq!(cursor.dequeue_index(), 0);
    // Second pass: the controller now produces with cycle 0; stale
    // first-pass TRBs (cycle 1) must not be re-consumed.
    assert_eq!(cursor.pop(&segment), Ok(None));
    segment[0] = event(false);
    assert!(cursor.pop(&segment).unwrap().is_some());
}

#[test]
fn event_cursor_rejects_wrong_segment() {
    let segment = [Trb::ZERO; 3];
    let mut cursor = EventRingCursor::new(4).expect("cursor fits");
    assert_eq!(cursor.pop(&segment), Err(DriverError::LengthOutOfRange));
}

#[test]
fn trb_bytes_round_trip() {
    let trb = Trb {
        parameter: 0x1122_3344_5566_7788,
        status: 0xAABB_CCDD,
        control: 0x0102_0304,
    };
    assert_eq!(Trb::from_bytes(trb.to_bytes()), trb);
    assert_eq!(trb.to_bytes()[0], 0x88, "little-endian on the ring");
}

#[test]
fn transfer_event_field_helpers() {
    let event = Trb {
        parameter: 0x2000,
        status: (u32::from(CompletionCode::ShortPacket.as_u8()) << 24) | 5,
        control: (u32::from(TrbType::TransferEvent.as_u8()) << 10)
            | (3 << 16)
            | trb::control_slot(7),
    };
    assert_eq!(event.endpoint_id(), 3);
    assert_eq!(event.transfer_residual(), 5);
    assert_eq!(event.slot_id(), 7);
}

#[test]
fn device_descriptor_decode_fails_closed() {
    let descriptor = DeviceDescriptor::decode(&MOCK_DESCRIPTOR).expect("fixture decodes");
    assert_eq!(descriptor.vendor_id, 0x046D);
    assert_eq!(descriptor.product_id, 0xC077);
    assert_eq!(descriptor.device_class, 0);
    assert_eq!(descriptor.num_configurations, 1);

    let mut short_length = MOCK_DESCRIPTOR;
    short_length[0] = 17;
    assert_eq!(
        DeviceDescriptor::decode(&short_length),
        Err(DriverError::BadMagic)
    );
    let mut wrong_type = MOCK_DESCRIPTOR;
    wrong_type[1] = 0x02;
    assert_eq!(
        DeviceDescriptor::decode(&wrong_type),
        Err(DriverError::BadMagic)
    );
    let mut no_configs = MOCK_DESCRIPTOR;
    no_configs[17] = 0;
    assert_eq!(
        DeviceDescriptor::decode(&no_configs),
        Err(DriverError::BadMagic)
    );
}

#[test]
fn dma_program_rejects_unaligned_addresses() {
    let aligned = DmaProgram {
        dcbaap: 0x1000,
        command_ring: 0x1040,
        erst: 0x1080,
        event_segment: 0x10C0,
    };
    assert!(aligned.is_plausible());
    assert!(!DmaProgram {
        dcbaap: 0,
        ..aligned
    }
    .is_plausible());
    assert!(!DmaProgram {
        command_ring: 0x1044,
        ..aligned
    }
    .is_plausible());
    let mut xhci = Xhci::open(MockXhci::new()).expect("bring-up succeeds");
    assert_eq!(
        xhci.start(
            &DmaProgram {
                erst: 0x1004,
                ..aligned
            },
            16,
        ),
        Err(DriverError::OutOfRange)
    );
}

/// Open the mock controller and start the engine over the shared
/// buffer.
fn started_device(mock: MockXhci, mem: &SharedMem) -> UsbDevice<MockXhci, MockDma> {
    let xhci = Xhci::open(mock).expect("bring-up succeeds");
    let dma = MockDma {
        mem: Rc::clone(mem),
        phys: MOCK_DMA_BASE,
    };
    UsbDevice::start(xhci, dma, 4096).expect("engine starts")
}

#[test]
fn usb_device_start_programs_dma_and_runs() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    let mock = device.host_mut();
    assert_eq!(mock.usbcmd & regs::USBCMD_RUN, regs::USBCMD_RUN);
    assert_eq!(mock.config, 32, "all reported slots enabled");
    assert_eq!(MockXhci::qword(mock.dcbaap), MOCK_DMA_BASE);
    assert_eq!(
        MockXhci::qword(mock.crcr) & u64::from(regs::CRCR_RCS),
        1,
        "command ring starts at consumer cycle state 1"
    );
    assert_eq!(mock.erstsz, 1);
    // The single ERST entry names the event segment the initial ERDP
    // points at, sized in TRBs.
    let entry = mock.read_dwords(MockXhci::qword(mock.erstba), 4);
    let segment = (u64::from(entry[1]) << 32) | u64::from(entry[0]);
    assert_eq!(segment, MockXhci::qword(mock.erdp));
    assert_eq!(entry[2] as usize, RING_TRBS);
}

#[test]
fn usb_device_start_rejects_bad_regions() {
    let mem = shared_mem();
    let xhci = Xhci::open(MockXhci::with_device(&mem)).expect("bring-up succeeds");
    let misaligned = MockDma {
        mem: Rc::clone(&mem),
        phys: MOCK_DMA_BASE + 4,
    };
    assert!(matches!(
        UsbDevice::start(xhci, misaligned, 4096).err(),
        Some(DriverError::OutOfRange)
    ));

    let tiny = Rc::new(RefCell::new(alloc::vec![0u8; 256]));
    let xhci = Xhci::open(MockXhci::with_device(&tiny)).expect("bring-up succeeds");
    let small = MockDma {
        mem: Rc::clone(&tiny),
        phys: MOCK_DMA_BASE,
    };
    assert!(matches!(
        UsbDevice::start(xhci, small, 4096).err(),
        Some(DriverError::LengthOutOfRange)
    ));
}

#[test]
fn enumerate_hid_full_chain() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    let descriptor = device.enumerate_hid(1).expect("enumeration succeeds");
    assert_eq!(
        descriptor,
        DeviceDescriptor {
            vendor_id: 0x046D,
            product_id: 0xC077,
            device_class: 0,
            num_configurations: 1,
        }
    );
    assert_eq!(device.slot(), 1);
    let mock = device.host_mut();
    assert!(mock.addressed, "Address Device reached the model");
    assert!(mock.configured, "Configure Endpoint reached the model");
    assert_eq!(mock.configuration, Some(1), "SET_CONFIGURATION(1) issued");
    assert_eq!(mock.protocol, Some(0), "SET_PROTOCOL selected boot");
}

#[test]
fn enumerate_hid_resets_a_disabled_port() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    // Connected but not yet enabled: the USB2 shape before a reset.
    mock.portsc[0] &= !regs::PORTSC_PED;
    let mut device = started_device(mock, &mem);
    device.enumerate_hid(1).expect("reset then enumeration");
    let mock = device.host_mut();
    assert_ne!(mock.portsc[0] & regs::PORTSC_PED, 0, "port re-enabled");
    assert!(mock.configured);
}

#[test]
fn enumerate_hid_fails_closed_on_an_empty_port() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    assert_eq!(device.enumerate_hid(2), Err(DriverError::DeviceFault));
    assert_eq!(device.enumerate_hid(0), Err(DriverError::OutOfRange));
}

#[test]
fn enumerate_hid_twice_is_refused() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("first enumeration");
    assert_eq!(device.enumerate_hid(1), Err(DriverError::Busy));
}

#[test]
fn enumerate_first_connected_finds_the_populated_port() {
    // `with_device` connects a device on root-hub port 1 and leaves the
    // others empty; the scan enumerates port 1 and lands on slot 1.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    let descriptor = device
        .enumerate_first_connected()
        .expect("port 1 is connected");
    assert_eq!(descriptor.vendor_id, 0x046D);
    assert_eq!(device.slot(), 1);
    assert!(device.host_mut().configured);
}

#[test]
fn enumerate_first_connected_fails_closed_on_an_empty_root_hub() {
    // No port reports a connected device: the scan refuses with
    // `NotFound` rather than guessing a port (`AGENTS.md` §2.9 / §5.4).
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    mock.portsc[0] &= !regs::PORTSC_CCS;
    let mut device = started_device(mock, &mem);
    assert_eq!(
        device.enumerate_first_connected(),
        Err(DriverError::NotFound)
    );
    assert_eq!(device.slot(), 0, "no device was enumerated");
}

#[test]
fn enumerate_hid_fails_closed_on_a_stalled_class_request() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    mock.stall_class_requests = true;
    let mut device = started_device(mock, &mem);
    assert_eq!(device.enumerate_hid(1), Err(DriverError::DeviceFault));
}

#[test]
fn reports_flow_through_the_report_source() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");

    let mock = device.host_mut();
    mock.pending_reports
        .push_back(alloc::vec![0, 0, 0x04, 0, 0, 0, 0, 0]);
    mock.pending_reports
        .push_back(alloc::vec![0x01, 0xFF, 0x02]);
    mock.process_int_ring();

    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(&mut buf), Ok(Some(REPORT_LEN)));
    assert_eq!(buf, [0, 0, 0x04, 0, 0, 0, 0, 0]);
    // The 3-byte mouse report arrives as a short packet.
    assert_eq!(device.next_report(&mut buf), Ok(Some(3)));
    assert_eq!(buf[..3], [0x01, 0xFF, 0x02]);
    assert_eq!(device.next_report(&mut buf), Ok(None));
}

#[test]
fn report_source_rearms_across_the_ring_wrap() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");

    // More reports than the primed TRBs and than the ring's data
    // slots: draining them all proves retire + re-arm keep the ring
    // live across the Link-TRB wrap.
    let total = 2 * RING_TRBS;
    assert!(total > PRIMED_REPORTS);
    let mock = device.host_mut();
    for index in 0..total {
        let marker = u8::try_from(index).expect("small index");
        mock.pending_reports
            .push_back(alloc::vec![marker, 0, 0, 0, 0, 0, 0, 0]);
    }
    mock.process_int_ring();

    let mut buf = [0u8; REPORT_LEN];
    for index in 0..total {
        let marker = u8::try_from(index).expect("small index");
        assert_eq!(device.next_report(&mut buf), Ok(Some(REPORT_LEN)));
        assert_eq!(buf[0], marker, "reports arrive in order");
    }
    assert_eq!(device.next_report(&mut buf), Ok(None));
}

#[test]
fn next_report_before_enumeration_fails_closed() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(&mut buf), Err(DriverError::DeviceFault));
}

#[test]
fn forged_report_residual_fails_closed() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");
    let mock = device.host_mut();
    mock.forge_report_residual = true;
    mock.pending_reports
        .push_back(alloc::vec![0, 0, 0x04, 0, 0, 0, 0, 0]);
    mock.process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(&mut buf), Err(DriverError::DeviceFault));
}

#[test]
fn boot_keyboard_decodes_over_the_xhci_transfer_ring() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");
    // Left Shift held plus key usage 0x04 (`A`).
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x02, 0, 0x04, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();

    let mut keyboard = BootKeyboard::new(device);
    let zero = rustos_abi::driver::input::InputEvent {
        kind: rustos_abi::driver::input::InputEventKind::Key,
        reserved0: 0,
        code: 0,
        value: 0,
    };
    let mut events = [zero; 4];
    let drained = keyboard.poll(&mut events).expect("poll succeeds");
    assert_eq!(drained, 2);
    assert_eq!(events[0].code, 0x04, "key press decoded");
    assert_eq!(events[0].value, 1);
    assert_eq!(events[1].code, 0xE1, "left-shift modifier edge");
    assert_eq!(events[1].value, 1);
    assert_eq!(keyboard.poll(&mut events), Ok(0));
}

#[test]
fn bind_table_class_matches_any_xhci_controller() {
    use rustos_abi::HwMatchKey;
    // One class-wildcard key: the VL805 (and any other xHCI host) binds
    // by class alone, whatever its vendor/device id.
    assert_eq!(BIND_KEYS.len(), 1);
    assert_eq!(BIND_KEYS[0].priority, BIND_PRIORITY);
    let vl805 = HwMatchKey::pci(0x1106, 0x3483, XHCI_PCI_CLASS);
    let other = HwMatchKey::pci(0x8086, 0x1111, XHCI_PCI_CLASS);
    assert!(BIND_KEYS[0].key.matches(&vl805));
    assert!(BIND_KEYS[0].key.matches(&other));
    // A USB controller of a different prog-if (EHCI, `0x0C0320`) is not
    // an xHCI host and must not bind.
    let ehci = HwMatchKey::pci(0x1106, 0x3483, 0x0C_03_20);
    assert!(!BIND_KEYS[0].key.matches(&ehci));
}
