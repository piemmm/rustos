//! Unit tests for the xHCI protocol layers against a register-level
//! mock controller (mirrors the `emmc2` `MockSdhci` seam).

extern crate alloc;

use alloc::vec::Vec;

use super::ring::{EventRingCursor, ProducerRing};
use super::trb::{CompletionCode, Trb, TrbType, CONTROL_CYCLE, TRB_LEN};
use super::*;
use rustos_abi::driver::DriverKind;

/// The mock's `CAPLENGTH` (so its operational base).
const MOCK_CAPLENGTH: u32 = 0x20;
/// The mock's doorbell-array offset.
const MOCK_DBOFF: u32 = 0x1000;
/// The mock's runtime-block offset.
const MOCK_RTSOFF: u32 = 0x2000;
/// The mock's register-window byte length.
const MOCK_WINDOW_LEN: usize = 0x3000;

/// Register-level xHCI model: the capability block, `USBCMD`/`USBSTS`
/// halt/reset behaviour, four `PORTSC` ports, and a doorbell write log.
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
        }
    }

    fn op(offset: usize) -> usize {
        MOCK_CAPLENGTH as usize + offset
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
        let portsc_base = Self::op(regs::PORTSC_BASE);
        for (port, &value) in self.portsc.iter().enumerate() {
            if offset == portsc_base + port * regs::PORTSC_STRIDE {
                return Ok(value);
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
        let db_base = MOCK_DBOFF as usize;
        if offset >= db_base && offset < db_base + 256 * 4 {
            self.doorbells.push((offset - db_base, value));
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

#[test]
fn producer_ring_rejects_tiny_rings() {
    let mut trbs = [Trb::ZERO; 2];
    assert!(matches!(
        ProducerRing::new(&mut trbs, 0x1000),
        Err(DriverError::LengthOutOfRange)
    ));
}

#[test]
fn producer_ring_stamps_cycle_and_reports_addresses() {
    let mut trbs = [Trb::ZERO; 4];
    {
        let mut ring = ProducerRing::new(&mut trbs, 0x1000).expect("ring fits");
        let a = ring.push(Trb::new(TrbType::NoOpCommand, 0, 0, 0)).unwrap();
        let b = ring.push(Trb::new(TrbType::NoOpCommand, 0, 0, 0)).unwrap();
        assert_eq!(a, 0x1000);
        assert_eq!(b, 0x1000 + TRB_LEN as u64);
        assert_eq!(ring.in_flight(), 2);
    }
    // First-pass TRBs carry cycle 1; the link TRB is still unpublished.
    assert!(trbs[0].cycle());
    assert!(trbs[1].cycle());
    assert_eq!(trbs[3].trb_type(), Ok(TrbType::Link));
    assert!(!trbs[3].cycle());
}

#[test]
fn producer_ring_rejects_caller_owned_fields() {
    let mut trbs = [Trb::ZERO; 4];
    let mut ring = ProducerRing::new(&mut trbs, 0x1000).expect("ring fits");
    assert_eq!(
        ring.push(Trb::new(TrbType::NoOpCommand, 0, 0, CONTROL_CYCLE)),
        Err(DriverError::OutOfRange)
    );
    assert_eq!(
        ring.push(Trb::new(TrbType::Link, 0x1000, 0, 0)),
        Err(DriverError::OutOfRange)
    );
}

#[test]
fn producer_ring_full_fails_closed_and_retire_reopens() {
    let mut trbs = [Trb::ZERO; 4];
    let mut ring = ProducerRing::new(&mut trbs, 0x1000).expect("ring fits");
    let no_op = Trb::new(TrbType::NoOpCommand, 0, 0, 0);
    ring.push(no_op).expect("slot 0");
    ring.push(no_op).expect("slot 1");
    assert_eq!(ring.push(no_op), Err(DriverError::Busy));
    ring.retire_one().expect("one completion");
    ring.push(no_op).expect("freed slot");
    assert_eq!(ring.retire_one(), Ok(()));
    assert_eq!(ring.retire_one(), Ok(()));
    assert_eq!(ring.retire_one(), Err(DriverError::OutOfRange));
}

#[test]
fn producer_ring_wrap_publishes_link_and_toggles_cycle() {
    let mut trbs = [Trb::ZERO; 4];
    {
        let mut ring = ProducerRing::new(&mut trbs, 0x1000).expect("ring fits");
        let no_op = Trb::new(TrbType::NoOpCommand, 0, 0, 0);
        ring.push(no_op).expect("slot 0");
        ring.push(no_op).expect("slot 1");
        ring.retire_one().expect("completion 0");
        ring.retire_one().expect("completion 1");
        // Third push lands in slot 2 — the last data slot — publishing
        // the link TRB under cycle 1 and toggling the producer to
        // cycle 0.
        let c = ring.push(no_op).expect("slot 2 wraps");
        assert_eq!(c, 0x1000 + 2 * TRB_LEN as u64);
        // Fourth push lands back in slot 0 under the toggled cycle.
        let d = ring.push(no_op).expect("slot 0 second pass");
        assert_eq!(d, 0x1000);
    }
    assert!(trbs[3].cycle(), "link TRB published under cycle 1");
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
