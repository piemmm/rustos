//! Host tests for the GENET v5 engine, driven against a register-level model
//! of the controller.
//!
//! QEMU models no GENET, so this suite is the device's coverage: it asserts
//! the bring-up sequence writes what the register map says, that the MDIO and
//! PHY paths frame and resolve correctly and fail closed on a dead bus, and
//! that the frame path moves, drops, and back-pressures exactly as the
//! [`Net`] contract requires.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cell::Cell;
use core::ptr::NonNull;

use tairix_abi::driver::dma::{DmaSlab, PoolId};
use tairix_abi::driver::net_ring::{FrameRings, RingGeometry};
use tairix_abi::driver::BufferClass;

use super::*;

/// The revision word a GENET v5 core reports: major nibble `6`, an arbitrary
/// minor and patch level.
const REV_V5: u32 = (regs::GENET_V5_MAJOR << regs::REV_MAJOR_SHIFT) | (0x1 << 16) | 0x1234;

/// A representative board MAC.
const MAC: [u8; 6] = [0xDC, 0xA6, 0x32, 0x11, 0x22, 0x33];

/// Device-visible base of the model's frame-buffer carve.
const FRAMES_PHYS: u64 = 0x3000_0000;

/// A model of the controller's register file.
///
/// Registers read back what was written, except for the handful with real
/// hardware behaviour: the MDIO command register completes its transaction on
/// the read that follows the start, and the PHY registers behind it answer
/// from `phy`. Every write is also appended to `writes` so a test can assert
/// the *order* of a bring-up sequence, not just its final state.
struct MockRegs {
    words: BTreeMap<usize, u32>,
    writes: Vec<(usize, u32)>,
    /// The clause-22 PHY register file the MDIO master reaches.
    phy: BTreeMap<u8, u16>,
    /// When set, an MDIO transaction never clears `START_BUSY` — a dead bus.
    mdio_hangs: bool,
    /// When set, an MDIO read reports that no PHY answered.
    mdio_read_fails: bool,
    /// Offsets past this are outside the modelled aperture.
    len: usize,
}

impl MockRegs {
    fn new() -> Self {
        let mut phy = BTreeMap::new();
        // A PHY that has finished negotiating a gigabit full-duplex link.
        phy.insert(0x01, (1 << 5) | (1 << 2));
        phy.insert(0x0A, 1 << 11);
        let mut words = BTreeMap::new();
        words.insert(regs::SYS_REV_CTRL, REV_V5);
        Self {
            words,
            writes: Vec::new(),
            phy,
            mdio_hangs: false,
            mdio_read_fails: false,
            len: 0x1_0000,
        }
    }

    /// Set the PHY's advertised link-partner state to "link down".
    fn phy_link_down(&mut self) {
        self.phy.insert(0x01, 0);
    }

    /// The value a register currently holds.
    fn peek(&self, offset: usize) -> u32 {
        self.words.get(&offset).copied().unwrap_or(0)
    }

    /// Every value written to `offset`, in order.
    fn writes_to(&self, offset: usize) -> Vec<u32> {
        self.writes
            .iter()
            .filter(|(o, _)| *o == offset)
            .map(|(_, v)| *v)
            .collect()
    }

    /// Position of the first write to `offset` in the whole write log, so a
    /// test can assert two writes happened in the required order.
    fn first_write_index(&self, offset: usize) -> Option<usize> {
        self.writes.iter().position(|(o, _)| *o == offset)
    }

    /// Pretend the device raised the interrupt sources in `bits`.
    fn assert_source(&mut self, bits: u32) {
        self.words.insert(regs::INTRL2_CPU_STAT, bits);
    }

    /// Pretend the device completed `count` receive descriptors.
    fn set_rx_produced(&mut self, count: u32) {
        let ring = regs::ring_regs(regs::RDMA_DESC, regs::DEFAULT_RING);
        self.words.insert(ring + regs::RING_DEVICE_INDEX, count);
    }

    /// Pretend the device consumed `count` transmit descriptors.
    fn set_tx_consumed(&mut self, count: u32) {
        let ring = regs::ring_regs(regs::TDMA_DESC, regs::DEFAULT_RING);
        self.words.insert(ring + regs::RING_DEVICE_INDEX, count);
    }

    /// Fill in a completed receive descriptor's status word.
    fn set_rx_desc(&mut self, slot: u32, length_status: u32) {
        let desc = regs::desc(regs::RDMA_DESC, slot);
        self.words
            .insert(desc + regs::DESC_LENGTH_STATUS, length_status);
    }

    /// Raise `sources` on the level-2 interrupt controller.
    fn assert_irq(&mut self, sources: u32) {
        self.words.insert(regs::INTRL2_CPU_STAT, sources);
    }

    /// Complete an MDIO transaction: clear `START_BUSY` and, for a read,
    /// substitute the addressed PHY register's value.
    fn complete_mdio(&mut self) {
        let command = self.peek(regs::MDIO_CMD);
        if command & regs::MDIO_START_BUSY == 0 || self.mdio_hangs {
            return;
        }
        let reg = u8::try_from((command >> regs::MDIO_REG_SHIFT) & regs::MDIO_REG_MASK)
            .expect("5-bit register number");
        let mut done = command & !regs::MDIO_START_BUSY;
        if command & regs::MDIO_RD == regs::MDIO_RD {
            if self.mdio_read_fails {
                done |= regs::MDIO_READ_FAIL;
            } else {
                done = (done & !regs::MDIO_DATA_MASK)
                    | u32::from(self.phy.get(&reg).copied().unwrap_or(0));
            }
        } else {
            // A write lands in the PHY's register file. A self-clearing
            // reset bit is already clear by the time the driver reads back.
            let value = u16::try_from(command & regs::MDIO_DATA_MASK).expect("16-bit data");
            self.phy.insert(reg, value & !(1 << 15));
        }
        self.words.insert(regs::MDIO_CMD, done);
    }
}

impl GenetRegs for MockRegs {
    fn read(&mut self, offset: usize) -> Result<u32, DriverError> {
        if offset + 4 > self.len {
            return Err(DriverError::OutOfRange);
        }
        if offset == regs::MDIO_CMD {
            self.complete_mdio();
        }
        Ok(self.peek(offset))
    }

    fn write(&mut self, offset: usize, value: u32) -> Result<(), DriverError> {
        if offset + 4 > self.len {
            return Err(DriverError::OutOfRange);
        }
        self.words.insert(offset, value);
        self.writes.push((offset, value));
        Ok(())
    }
}

/// A `Delay` whose clock advances by the requested interval, so a bounded
/// poll loop reaches its deadline in finite test time without sleeping.
struct MockDelay {
    now: Cell<u64>,
}

impl MockDelay {
    fn new() -> Self {
        Self { now: Cell::new(0) }
    }
}

impl tairix_abi::driver::timing::Delay for MockDelay {
    fn delay_us(&self, us: u32) {
        self.now.set(self.now.get() + u64::from(us));
    }

    fn now_us(&self) -> u64 {
        self.now.get()
    }
}

/// The machine every test derives its carve layout from: a 1 GiB, 4-core
/// board, so the depths the tests assert are the ones the policy produces
/// rather than a figure written down twice.
fn test_machine() -> BootFacts {
    BootFacts {
        arch: tairix_abi::Arch::Aarch64,
        cpu_name: tairix_abi::CpuName::UNKNOWN,
        cpu_count: 4,
        memory_bytes: 1024 * 1024 * 1024,
    }
}

/// The carve layout of [`test_machine`].
fn layout() -> DmaLayout {
    DmaLayout::for_machine(Some(&test_machine()))
}

/// Bytes the frame buffers of [`layout`] occupy — where the staging area
/// begins.
fn buffer_bytes() -> usize {
    layout().bytes() - layout().tx_staging() as usize
}

/// A leaked frame-buffer carve of `len` bytes standing in for the kernel's
/// DMA region.
fn frames_of(len: usize) -> DmaSlab {
    let storage = alloc::vec![0u8; len].leak();
    let ptr = NonNull::new(storage.as_mut_ptr()).expect("leaked storage is non-null");
    // SAFETY: `storage` is a `'static` leaked allocation of exactly `len`
    // bytes that nothing else references, and `FRAMES_PHYS` stands in for its
    // device-visible base in this host model.
    unsafe { DmaSlab::from_leaked(FRAMES_PHYS, ptr, len, PoolId::MOCK, 0) }
}

/// A leaked frame-buffer carve sized for [`layout`].
fn frames() -> DmaSlab {
    frames_of(layout().bytes())
}

/// Bring a device up over a fresh mock, returning both so a test can inspect
/// the register file the engine wrote.
fn open() -> Genet<MockRegs, MockDelay> {
    Genet::open(
        MockRegs::new(),
        MockDelay::new(),
        frames(),
        MacAddress::new(MAC),
        layout(),
    )
    .expect("bring-up succeeds against a GENET v5 model")
}

/// A ring geometry sized exactly as the stack derives it from the reported
/// facts.
fn geometry() -> RingGeometry {
    let capacity = MTU + ETHERNET_HEADER_LEN;
    RingGeometry::new(8, 8, capacity, capacity, 1).expect("valid geometry")
}

// --- bring-up -----------------------------------------------------------

#[test]
fn a_foreign_core_revision_is_refused() {
    // The matched node claimed a GENET v5; a core reporting anything else is
    // not the device this register layout describes, so bring-up refuses
    // rather than programming a foreign block.
    for major in [0, 1, 5, 7, 0xF] {
        let mut mock = MockRegs::new();
        mock.words
            .insert(regs::SYS_REV_CTRL, major << regs::REV_MAJOR_SHIFT);
        assert_eq!(
            Genet::open(
                mock,
                MockDelay::new(),
                frames(),
                MacAddress::new(MAC),
                layout()
            )
            .err(),
            Some(DriverError::Unsupported),
            "major {major} must be refused"
        );
    }
}

#[test]
fn the_dma_carve_scales_with_the_discovered_machine() {
    // The descriptor depth and the segmentation staging area are derived,
    // not hand-picked: a bigger board gets a deeper ring, the controller's
    // own descriptor RAM is the ceiling, and an unattested machine gets the
    // structural floor rather than an invented figure.
    let small = DmaLayout::for_machine(Some(&BootFacts {
        cpu_count: 1,
        memory_bytes: 128 * 1024 * 1024,
        ..test_machine()
    }));
    let big = DmaLayout::for_machine(Some(&BootFacts {
        cpu_count: 4,
        memory_bytes: 8 * 1024 * 1024 * 1024,
        ..test_machine()
    }));
    assert!(small.ring_slots() < layout().ring_slots());
    assert!(layout().ring_slots() < big.ring_slots());
    assert!(big.ring_slots() <= regs::TOTAL_DESC);
    assert!(small.tx_staging() < big.tx_staging());
    let floor = DmaLayout::for_machine(None);
    assert!(floor.ring_slots() < small.ring_slots());
    assert!(floor.bytes() > 0);
    // Every depth is a power of two, as the descriptor-ring size field and
    // the transport's slot masking both need.
    for l in [floor, small, layout(), big] {
        assert!(l.ring_slots().is_power_of_two());
        assert!(l.tx_staging() >= MTU + ETHERNET_HEADER_LEN);
    }
}

#[test]
fn a_short_dma_carve_is_refused() {
    let short = frames_of(layout().bytes() - 1);
    assert_eq!(
        Genet::open(
            MockRegs::new(),
            MockDelay::new(),
            short,
            MacAddress::new(MAC),
            layout()
        )
        .err(),
        Some(DriverError::BufferTooSmall)
    );
}

#[test]
fn a_short_register_window_fails_closed() {
    // A window that cannot reach the transmit descriptor RAM is a
    // mis-provisioned node: refuse rather than drive a truncated aperture.
    let mut mock = MockRegs::new();
    mock.len = 0x1000;
    assert_eq!(
        Genet::open(
            mock,
            MockDelay::new(),
            frames(),
            MacAddress::new(MAC),
            layout()
        )
        .err(),
        Some(DriverError::OutOfRange)
    );
}

#[test]
fn bring_up_masks_interrupts_before_it_programs_the_device() {
    let device = open();
    let mock = &device.regs;
    // Every level-2 source is masked and cleared before the rings are built,
    // so a condition left asserted by whatever ran before cannot storm the
    // line mid-bring-up.
    assert_eq!(
        mock.writes_to(regs::INTRL2_CPU_MASK_SET),
        [regs::INTRL2_ALL]
    );
    let masked = mock
        .first_write_index(regs::INTRL2_CPU_MASK_SET)
        .expect("mask-all written");
    let ring_cfg = mock
        .first_write_index(regs::dma_regs(regs::RDMA_DESC) + regs::DMA_RING_CFG)
        .expect("receive ring configured");
    assert!(
        masked < ring_cfg,
        "sources masked before the rings are armed"
    );
    // Only the four sources the driver handles end up enabled.
    assert_eq!(
        mock.writes_to(regs::INTRL2_CPU_MASK_CLEAR),
        [regs::IRQ_ENABLED]
    );
}

#[test]
fn masking_the_completion_sources_leaves_the_link_sources_live() {
    let mut device = open();
    // Masking is the half of the poll-to-budget cycle that stops the storm:
    // the DMA-done bits latch a level condition that is still asserted
    // while frames are undrained, so acknowledging alone re-fires at once.
    device
        .set_completion_interrupts(false)
        .expect("mask the completion sources");
    let masked = device
        .regs
        .writes_to(regs::INTRL2_CPU_MASK_SET)
        .last()
        .copied()
        .expect("a mask write");
    assert_eq!(masked, regs::IRQ_COMPLETION);
    assert_eq!(
        masked & (regs::IRQ_LINK_UP | regs::IRQ_LINK_DOWN),
        0,
        "a cable pulled mid-flood must still wake the driver"
    );

    device
        .set_completion_interrupts(true)
        .expect("unmask the completion sources");
    assert_eq!(
        device
            .regs
            .writes_to(regs::INTRL2_CPU_MASK_CLEAR)
            .last()
            .copied(),
        Some(regs::IRQ_COMPLETION),
        "unmasking touches exactly the sources masking did"
    );
}

#[test]
fn acknowledging_an_interrupt_does_not_unmask_anything() {
    let mut device = open();
    device
        .set_completion_interrupts(false)
        .expect("mask the completion sources");
    let clears_before = device.regs.writes_to(regs::INTRL2_CPU_MASK_CLEAR).len();
    device.regs.assert_source(regs::IRQ_RXDMA_DONE);
    device.ack_interrupt();
    // Acknowledgement clears the latch only. Re-arming the source while the
    // condition is still asserted is what spins the driver, so it is the
    // drain's decision, never the acknowledgement's.
    assert_eq!(
        device.regs.writes_to(regs::INTRL2_CPU_MASK_CLEAR).len(),
        clears_before
    );
    assert_eq!(
        device
            .regs
            .writes_to(regs::INTRL2_CPU_CLEAR)
            .last()
            .copied(),
        Some(regs::IRQ_RXDMA_DONE)
    );
}

#[test]
fn bring_up_resets_the_mac_and_programs_its_identity() {
    let device = open();
    let mock = &device.regs;
    // The reset pulse holds local loopback so no partial frame escapes.
    assert_eq!(
        mock.writes_to(regs::UMAC_CMD).first().copied(),
        Some(0),
        "the command register is quiesced first"
    );
    assert!(mock
        .writes_to(regs::UMAC_CMD)
        .contains(&(regs::CMD_SW_RESET | regs::CMD_LCL_LOOP_EN)));
    // The statistics counters are reset and released.
    assert_eq!(
        mock.writes_to(regs::UMAC_MIB_CTRL),
        [
            regs::MIB_RESET_RX | regs::MIB_RESET_TX | regs::MIB_RESET_RUNT,
            0
        ]
    );
    // The board MAC, big-endian across the two address registers.
    assert_eq!(mock.peek(regs::UMAC_MAC0), 0xDCA6_3211);
    assert_eq!(mock.peek(regs::UMAC_MAC1), 0x2233);
    // The receive buffer inserts its two-byte alignment pad, the frame limit
    // covers every header the MAC may see, and the port is external RGMII.
    assert!(mock.peek(regs::RBUF_CTRL) & regs::RBUF_ALIGN_2B != 0);
    assert_eq!(mock.peek(regs::UMAC_MAX_FRAME_LEN), MAX_FRAME_LEN);
    assert_eq!(mock.peek(regs::SYS_PORT_CTRL), regs::PORT_MODE_EXT_GPHY);
    // The frame check sequence is stripped by the MAC, so a descriptor's
    // reported length is the frame length.
    assert_eq!(
        mock.peek(regs::UMAC_CMD) & (1 << 6),
        0,
        "CRC_FWD stays clear"
    );
}

#[test]
fn bring_up_admits_the_station_address_and_broadcast_through_the_receive_filter() {
    let device = open();
    let mock = &device.regs;
    // The address registers identify the station; the destination-address
    // filter is what admits a frame. Both slots must be programmed *and*
    // enabled, or the receiver delivers nothing: no ARP, no DHCP offer, no
    // unicast reply.
    let broadcast = *MacAddress::BROADCAST.as_octets();
    let station = MAC;
    for (slot, address) in [broadcast, station].iter().enumerate() {
        let base = regs::UMAC_MDF_ADDR + slot * regs::MDF_SLOT_STRIDE;
        assert_eq!(
            mock.peek(base),
            u32::from(u16::from_be_bytes([address[0], address[1]])),
            "slot {slot} high half"
        );
        assert_eq!(
            mock.peek(base + 4),
            u32::from_be_bytes([address[2], address[3], address[4], address[5]]),
            "slot {slot} low word"
        );
    }
    // Exactly those two slots are enabled — slot 0 by the top bit, slot 1 by
    // the next — and no third address is admitted.
    let top = 1u32 << (regs::MDF_SLOTS - 1);
    assert_eq!(mock.peek(regs::UMAC_MDF_CTRL), top | (top >> 1));
    // Promiscuous reception is never enabled: the stack sees frames addressed
    // to this host, not everything on the segment.
    assert_eq!(
        mock.peek(regs::UMAC_CMD) & (1 << 4),
        0,
        "PROMISC stays clear"
    );
}

#[test]
fn group_addresses_are_admitted_after_the_two_fixed_slots() {
    let mut device = open();
    let groups = [
        MacAddress::new([0x33, 0x33, 0x00, 0x00, 0x00, 0x01]),
        MacAddress::new([0x01, 0x00, 0x5E, 0x00, 0x00, 0x01]),
    ];
    device
        .set_multicast_groups(&groups)
        .expect("two groups fit the table");
    let mock = &device.regs;
    // Broadcast and the station address keep the first two slots; the groups
    // follow, so a group can never displace what makes the host addressable.
    for (slot, address) in [
        MacAddress::BROADCAST,
        MacAddress::new(MAC),
        groups[0],
        groups[1],
    ]
    .iter()
    .enumerate()
    {
        let octets = address.as_octets();
        let base = regs::UMAC_MDF_ADDR + slot * regs::MDF_SLOT_STRIDE;
        assert_eq!(
            mock.peek(base),
            u32::from(u16::from_be_bytes([octets[0], octets[1]])),
            "slot {slot} high half"
        );
        assert_eq!(
            mock.peek(base + 4),
            u32::from_be_bytes([octets[2], octets[3], octets[4], octets[5]]),
            "slot {slot} low word"
        );
    }
    let top = 1u32 << (regs::MDF_SLOTS - 1);
    assert_eq!(
        mock.peek(regs::UMAC_MDF_CTRL),
        top | (top >> 1) | (top >> 2) | (top >> 3),
        "exactly four slots enabled"
    );
}

#[test]
fn replacing_the_group_set_disables_the_slots_it_dropped() {
    let mut device = open();
    let group = MacAddress::new([0x33, 0x33, 0x00, 0x00, 0x00, 0x01]);
    device.set_multicast_groups(&[group]).expect("one group");
    device
        .set_multicast_groups(&[])
        .expect("the empty set leaves the fixed pair");
    // A stale enable bit would keep admitting a group the stack has left.
    let top = 1u32 << (regs::MDF_SLOTS - 1);
    assert_eq!(device.regs.peek(regs::UMAC_MDF_CTRL), top | (top >> 1));
}

#[test]
fn a_group_set_larger_than_the_table_is_refused_whole() {
    let mut device = open();
    let group = MacAddress::new([0x33, 0x33, 0x00, 0x00, 0x00, 0x01]);
    device.set_multicast_groups(&[group]).expect("one group");
    let enabled_before = device.regs.peek(regs::UMAC_MDF_CTRL);
    let too_many: Vec<MacAddress> = (0..=u32::from(MCAST_SLOTS))
        .map(|n| {
            let o = n.to_be_bytes();
            MacAddress::new([0x33, 0x33, o[0], o[1], o[2], o[3]])
        })
        .collect();
    assert_eq!(
        device.set_multicast_groups(&too_many),
        Err(DriverError::LengthOutOfRange),
        "one past the table is refused"
    );
    // Refused whole: the working set stays, and the filter is never widened
    // to make an over-large set fit.
    assert_eq!(device.regs.peek(regs::UMAC_MDF_CTRL), enabled_before);
}

#[test]
fn every_service_report_states_the_live_link() {
    // The stack reads the link from the report on every doorbell and turns
    // a change into a bond failover; a report that always said `Up` would
    // pin the interface up for ever however the wire actually looked.
    let mut device = open();
    let mut region = alloc::vec![0u8; geometry().region_len()];
    assert_eq!(
        service(&mut device, &mut region, BufferClass::NonSensitive).link,
        LinkState::Up
    );

    device.regs.phy_link_down();
    device.regs.assert_irq(regs::IRQ_LINK_DOWN);
    device.ack_interrupt();
    assert_eq!(
        service(&mut device, &mut region, BufferClass::NonSensitive).link,
        LinkState::Down
    );
}

#[test]
fn both_rings_have_their_completion_interrupt_armed_before_the_mac_runs() {
    // The condition that raises `IRQ_*DMA_DONE` is a comparison against the
    // ring's done threshold, so a ring left holding whatever value reset put
    // there has unknown interrupt behaviour — and a threshold of zero is
    // satisfied permanently, which is a level condition draining cannot
    // clear and so a storm no masking can end.
    let device = open();
    let mock = &device.regs;
    for desc_base in [regs::RDMA_DESC, regs::TDMA_DESC] {
        let ring = regs::ring_regs(desc_base, regs::DEFAULT_RING);
        assert_eq!(
            mock.peek(ring + regs::RING_MBUF_DONE_THRESH),
            1,
            "one completed descriptor raises the source; the drain's masking is the coalescer"
        );
        let timeout = regs::ring_timeout(desc_base, regs::DEFAULT_RING);
        assert!(
            mock.first_write_index(timeout).is_some(),
            "the ring timer is programmed, not inherited"
        );
        assert_eq!(
            mock.peek(timeout) & regs::DMA_TIMEOUT_MASK,
            0,
            "disarmed: a timer would only add latency to a lone frame"
        );
        // Both must be in force before the DMA engine can complete anything.
        let armed = mock
            .first_write_index(ring + regs::RING_MBUF_DONE_THRESH)
            .expect("threshold programmed");
        let dma_enabled = mock
            .writes
            .iter()
            .position(|(offset, value)| {
                *offset == regs::dma_regs(desc_base) + regs::DMA_CTRL && value & regs::DMA_EN != 0
            })
            .expect("bring-up enables the engine");
        assert!(armed < dma_enabled);
    }
}

#[test]
fn bring_up_masks_the_interrupt_instance_it_never_services() {
    // This driver drives only the default ring, so it never binds
    // `INTRL2_1`'s line and never services it. Leaving its sources live
    // would leave part of the device's interrupt state to whatever the
    // firmware left behind.
    let device = open();
    let mock = &device.regs;
    assert_eq!(
        mock.writes_to(regs::INTRL2_1_CPU_MASK_SET),
        alloc::vec![regs::INTRL2_ALL]
    );
    let masked = mock
        .first_write_index(regs::INTRL2_1_CPU_MASK_SET)
        .expect("masked during bring-up");
    let rx_enabled = mock
        .writes
        .iter()
        .position(|(offset, value)| *offset == regs::UMAC_CMD && value & regs::CMD_RX_EN != 0)
        .expect("bring-up enables the receiver");
    assert!(masked < rx_enabled);
}

#[test]
fn the_device_reports_the_group_slots_it_has_left() {
    let device = open();
    let facts = device.device_facts().expect("facts");
    assert_eq!(facts.multicast_filter, McastFilter::Slots(MCAST_SLOTS));
    assert_eq!(MCAST_SLOTS, regs::MDF_SLOTS - RX_FILTER_ADDRESSES);
}

#[test]
fn the_receive_filter_is_programmed_before_the_receiver_is_enabled() {
    let device = open();
    let mock = &device.regs;
    let filter = mock
        .first_write_index(regs::UMAC_MDF_CTRL)
        .expect("the receive filter is programmed during bring-up");
    // `apply_link` is what sets CMD_RX_EN; a filter written after it would
    // leave a window in which the receiver drops every arriving frame.
    let rx_enabled = mock
        .writes
        .iter()
        .position(|(offset, value)| *offset == regs::UMAC_CMD && value & regs::CMD_RX_EN != 0)
        .expect("bring-up enables the receiver");
    assert!(filter < rx_enabled);
}

#[test]
fn bring_up_arms_both_rings_over_the_dma_carve() {
    let device = open();
    let phys = FRAMES_PHYS;
    let mock = &device.regs;
    for (desc_base, first_buffer) in [
        (regs::RDMA_DESC, phys),
        (
            regs::TDMA_DESC,
            phys + u64::from(layout().ring_slots()) * u64::from(BUF_LEN),
        ),
    ] {
        let ring = regs::ring_regs(desc_base, regs::DEFAULT_RING);
        assert_eq!(
            mock.peek(ring + regs::RING_BUF_SIZE),
            (layout().ring_slots() << regs::RING_SIZE_SHIFT) | BUF_LEN
        );
        assert_eq!(mock.peek(ring + regs::RING_START_ADDR), 0);
        assert_eq!(
            mock.peek(ring + regs::RING_END_ADDR),
            layout().ring_slots() * regs::DESC_WORDS - 1
        );
        assert_eq!(
            mock.peek(regs::dma_regs(desc_base) + regs::DMA_RING_CFG),
            1 << regs::DEFAULT_RING
        );
        // Both engines are enabled with the default ring's buffers.
        let ctrl = mock.peek(regs::dma_regs(desc_base) + regs::DMA_CTRL);
        assert!(ctrl & regs::DMA_EN != 0);
        assert!(ctrl & (1 << (regs::DEFAULT_RING + regs::DMA_RING_BUF_EN_SHIFT)) != 0);
        // Every descriptor points at its own buffer in the carve.
        for slot in 0..layout().ring_slots() {
            let desc = regs::desc(desc_base, slot);
            let expected = first_buffer + u64::from(slot) * u64::from(BUF_LEN);
            assert_eq!(
                u64::from(mock.peek(desc + regs::DESC_ADDRESS_LO)),
                expected & 0xFFFF_FFFF
            );
            assert_eq!(
                u64::from(mock.peek(desc + regs::DESC_ADDRESS_HI)),
                expected >> 32
            );
        }
    }
    // Receive descriptors start device-owned with the full buffer length;
    // transmit descriptors start idle.
    assert_eq!(
        mock.peek(regs::desc(regs::RDMA_DESC, 0) + regs::DESC_LENGTH_STATUS),
        (BUF_LEN << regs::DMA_BUFLENGTH_SHIFT) | regs::DMA_OWN
    );
    assert_eq!(
        mock.peek(regs::desc(regs::TDMA_DESC, 0) + regs::DESC_LENGTH_STATUS),
        0
    );
}

// --- MDIO and the PHY ---------------------------------------------------

#[test]
fn a_negotiated_gigabit_link_is_programmed_into_the_mac() {
    let device = open();
    assert_eq!(
        device.link,
        Some(mdio::Link {
            speed: mdio::LinkSpeed::Thousand,
            full_duplex: true
        })
    );
    let facts = device.device_facts().expect("facts");
    assert_eq!(facts.link, LinkState::Up);
    assert_eq!(facts.mac.as_octets(), &MAC);
    assert_eq!(facts.mtu, MTU);
    assert_eq!(facts.rx_queues, 1);
    // The receive-checksum verdict, the transmit-checksum engine for both
    // transports, and the driver-side segmentation built over it.
    assert!(facts.offloads.contains(NetOffloads::RX_CSUM_VALIDATED));
    assert!(facts.offloads.contains(NetOffloads::TX_CSUM_TCP));
    assert!(facts.offloads.contains(NetOffloads::TX_CSUM_UDP));
    assert!(facts.offloads.contains(NetOffloads::TX_SEGMENT_TCP));
    // The IPv4 header checksum has no engine on this MAC.
    assert!(!facts.offloads.contains(NetOffloads::TX_CSUM_IPV4));

    let cmd = device.regs.peek(regs::UMAC_CMD);
    assert_eq!(
        (cmd >> regs::CMD_SPEED_SHIFT) & regs::CMD_SPEED_MASK,
        regs::UMAC_SPEED_1000
    );
    assert_eq!(
        cmd & (regs::CMD_TX_EN | regs::CMD_RX_EN),
        regs::CMD_TX_EN | regs::CMD_RX_EN
    );
    // The board wires `rgmii-rxid`, so the MAC adds no transmit delay of its
    // own and drives the link indication itself.
    let oob = device.regs.peek(regs::EXT_RGMII_OOB_CTRL);
    assert_eq!(oob & regs::OOB_DISABLE, 0);
    assert!(oob & (regs::RGMII_LINK | regs::RGMII_MODE_EN | regs::ID_MODE_DIS) != 0);
}

#[test]
fn each_negotiated_rate_selects_its_own_mac_speed() {
    // Gigabit comes from the 1000BASE-T status register; 100 and 10 from the
    // link-partner ability register, full duplex preferred at each rate.
    for (gbsr, anlpar, expected, full_duplex) in [
        (1 << 11, 0, regs::UMAC_SPEED_1000, true),
        (1 << 10, 0, regs::UMAC_SPEED_1000, false),
        (0, 1 << 8, regs::UMAC_SPEED_100, true),
        (0, 1 << 7, regs::UMAC_SPEED_100, false),
        (0, 1 << 6, regs::UMAC_SPEED_10, true),
        (0, 1 << 5, regs::UMAC_SPEED_10, false),
    ] {
        let mut mock = MockRegs::new();
        mock.phy.insert(0x0A, gbsr);
        mock.phy.insert(0x05, anlpar);
        let device = Genet::open(
            mock,
            MockDelay::new(),
            frames(),
            MacAddress::new(MAC),
            layout(),
        )
        .expect("bring-up");
        assert_eq!(
            device.link.map(|l| l.full_duplex),
            Some(full_duplex),
            "gbsr {gbsr:#x} anlpar {anlpar:#x}"
        );
        assert_eq!(
            (device.regs.peek(regs::UMAC_CMD) >> regs::CMD_SPEED_SHIFT) & regs::CMD_SPEED_MASK,
            expected,
            "gbsr {gbsr:#x} anlpar {anlpar:#x}"
        );
    }
}

#[test]
fn no_link_partner_comes_up_down_with_the_mac_disabled() {
    // Autonegotiation that never completes is "no cable", not a failure: the
    // interface comes up link-down and the PHY's link-up interrupt resolves
    // it later.
    let mut mock = MockRegs::new();
    mock.phy_link_down();
    let device = Genet::open(
        mock,
        MockDelay::new(),
        frames(),
        MacAddress::new(MAC),
        layout(),
    )
    .expect("bring-up");
    assert_eq!(device.link, None);
    assert_eq!(device.device_facts().expect("facts").link, LinkState::Down);
    let cmd = device.regs.peek(regs::UMAC_CMD);
    assert_eq!(cmd & (regs::CMD_TX_EN | regs::CMD_RX_EN), 0);
}

#[test]
fn a_wedged_mdio_bus_fails_closed_rather_than_spinning() {
    let mut mock = MockRegs::new();
    mock.mdio_hangs = true;
    assert_eq!(
        Genet::open(
            mock,
            MockDelay::new(),
            frames(),
            MacAddress::new(MAC),
            layout()
        )
        .err(),
        Some(DriverError::DeviceFault)
    );
}

#[test]
fn an_absent_phy_fails_closed() {
    let mut mock = MockRegs::new();
    mock.mdio_read_fails = true;
    assert_eq!(
        Genet::open(
            mock,
            MockDelay::new(),
            frames(),
            MacAddress::new(MAC),
            layout()
        )
        .err(),
        Some(DriverError::DeviceFault)
    );
}

#[test]
fn mdio_frames_a_read_and_a_write_per_clause_22() {
    let mut mock = MockRegs::new();
    let delay = MockDelay::new();
    mock.phy.insert(0x04, 0xABCD);
    assert_eq!(
        mdio::read(&mut mock, &delay, mdio::PHY_ADDRESS, 0x04),
        Ok(0xABCD)
    );
    let posted = mock.writes_to(regs::MDIO_CMD);
    let command = posted[0];
    assert_eq!(command & regs::MDIO_RD, regs::MDIO_RD);
    assert_eq!(
        (command >> regs::MDIO_PMD_SHIFT) & regs::MDIO_PMD_MASK,
        u32::from(mdio::PHY_ADDRESS)
    );
    assert_eq!(
        (command >> regs::MDIO_REG_SHIFT) & regs::MDIO_REG_MASK,
        0x04
    );
    // The transaction is only started after the command is staged.
    assert_eq!(posted[1], command | regs::MDIO_START_BUSY);

    mock.writes.clear();
    mdio::write(&mut mock, &delay, mdio::PHY_ADDRESS, 0x09, 0x0300).expect("write");
    let command = mock.writes_to(regs::MDIO_CMD)[0];
    assert_eq!(command & regs::MDIO_WR, regs::MDIO_WR);
    assert_eq!(command & regs::MDIO_DATA_MASK, 0x0300);
    assert_eq!(mock.phy.get(&0x09).copied(), Some(0x0300));
}

#[test]
fn autonegotiation_advertises_every_rate_the_mac_can_carry() {
    let device = open();
    // 10/100 half and full in the advertisement register, both gigabit modes
    // in the 1000BASE-T control register, and negotiation enabled.
    let anar = device.regs.phy.get(&0x04).copied().expect("advertisement");
    assert_eq!(anar & 0x01E0, 0x01E0);
    let gbcr = device
        .regs
        .phy
        .get(&0x09)
        .copied()
        .expect("gigabit control");
    assert_eq!(gbcr & 0x0300, 0x0300);
    let bmcr = device.regs.phy.get(&0x00).copied().expect("control");
    assert!(bmcr & (1 << 12) != 0, "autonegotiation enabled");
    assert!(bmcr & (1 << 11) == 0, "not powered down");
}

// --- interrupts ---------------------------------------------------------

#[test]
fn acknowledging_an_interrupt_clears_exactly_what_was_asserted() {
    let mut device = open();
    device.regs.writes.clear();
    device
        .regs
        .assert_irq(regs::IRQ_RXDMA_DONE | regs::IRQ_TXDMA_DONE);
    device.ack_interrupt();
    assert_eq!(
        device.regs.writes_to(regs::INTRL2_CPU_CLEAR),
        [regs::IRQ_RXDMA_DONE | regs::IRQ_TXDMA_DONE]
    );
    // A DMA doorbell is not a link event.
    assert!(!device.link_event);

    // Nothing asserted: nothing written, so an idle wake cannot storm.
    device.regs.writes.clear();
    device.regs.assert_irq(0);
    device.ack_interrupt();
    assert!(device.regs.writes_to(regs::INTRL2_CPU_CLEAR).is_empty());
}

#[test]
fn a_link_event_re_resolves_the_link_on_the_next_service() {
    let mut device = open();
    assert!(device.link.is_some());

    // The cable is pulled: the PHY reports the link down and raises the
    // link-down source.
    device.regs.phy_link_down();
    device.regs.assert_irq(regs::IRQ_LINK_DOWN);
    device.ack_interrupt();
    assert!(device.link_event);

    let mut region = alloc::vec![0u8; geometry().region_len()];
    let mut rings =
        FrameRings::bind(&mut region, geometry(), BufferClass::NonSensitive).expect("bind");
    let report = device.service(&mut rings).expect("service");
    assert_eq!(device.link, None);
    assert!(!device.link_event, "the event is consumed");
    assert_eq!(
        report.link,
        LinkState::Down,
        "the doorbell is how the stack learns the cable was pulled"
    );
    assert_eq!(
        device.regs.peek(regs::UMAC_CMD) & (regs::CMD_TX_EN | regs::CMD_RX_EN),
        0,
        "a down link disables the MAC"
    );

    // Plugged back in: the next link event brings it up again.
    device.regs.phy.insert(0x01, (1 << 5) | (1 << 2));
    device.regs.assert_irq(regs::IRQ_LINK_UP);
    device.ack_interrupt();
    device.service(&mut rings).expect("service");
    assert!(device.link.is_some());
    assert_eq!(
        device.regs.peek(regs::UMAC_CMD) & (regs::CMD_TX_EN | regs::CMD_RX_EN),
        regs::CMD_TX_EN | regs::CMD_RX_EN
    );
}

// --- the frame path -----------------------------------------------------

/// Drive one service doorbell over a freshly-bound region.
fn service(
    device: &mut Genet<MockRegs, MockDelay>,
    region: &mut [u8],
    class: BufferClass,
) -> ServiceReport {
    let mut rings = FrameRings::bind(region, geometry(), class).expect("bind");
    device.service(&mut rings).expect("service")
}

#[test]
fn a_queued_frame_is_written_into_a_transmit_slot_and_rung_through() {
    let mut device = open();
    let mut region = alloc::vec![0u8; geometry().region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geometry(), BufferClass::NonSensitive).expect("bind");
        rings.tx.push(&[0xAB; 64]).expect("queue");
    }
    let report = service(&mut device, &mut region, BufferClass::NonSensitive);
    assert_eq!(report.transmitted, 1);

    // The frame reached slot 0's buffer past the transmit status block, its
    // descriptor names both lengths and asks the MAC to append the frame
    // check sequence, and the producer index advanced by exactly one.
    let (start, _) = Genet::<MockRegs, MockDelay>::tx_frame_range(layout(), 0);
    assert_eq!(&device.frames.as_bytes()[start..start + 64], &[0xAB; 64]);
    let status = device
        .regs
        .peek(regs::desc(regs::TDMA_DESC, 0) + regs::DESC_LENGTH_STATUS);
    assert_eq!(
        (status >> regs::DMA_BUFLENGTH_SHIFT) & regs::DMA_BUFLENGTH_MASK,
        regs::TSB_LEN + 64
    );
    assert!(status & regs::DMA_TX_APPEND_CRC != 0);
    assert_eq!(
        status & regs::DMA_TX_DO_CSUM,
        0,
        "a frame with no offload leaves the checksum engine idle"
    );
    assert_eq!(
        status & (regs::DMA_SOP | regs::DMA_EOP),
        regs::DMA_SOP | regs::DMA_EOP
    );
    let ring = regs::ring_regs(regs::TDMA_DESC, regs::DEFAULT_RING);
    assert_eq!(
        device.regs.writes_to(ring + regs::RING_DRIVER_INDEX).last(),
        Some(&1)
    );
}

#[test]
fn a_full_transmit_ring_stops_draining_without_loss() {
    let mut device = open();
    // A transmit ring deeper than the device's, so more frames are queued
    // than the device has descriptors for.
    let device_slots = layout().ring_slots();
    let queued = device_slots + 4;
    let geometry = RingGeometry::new(
        8,
        device_slots * 2,
        MTU + ETHERNET_HEADER_LEN,
        MTU + ETHERNET_HEADER_LEN,
        1,
    )
    .expect("geometry");
    let mut region = alloc::vec![0u8; geometry.region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geometry, BufferClass::NonSensitive).expect("bind");
        for _ in 0..queued {
            rings.tx.push(&[0x5A; 100]).expect("queue");
        }
    }
    let mut rings =
        FrameRings::bind(&mut region, geometry, BufferClass::NonSensitive).expect("bind");
    let report = device.service(&mut rings).expect("service");
    assert_eq!(report.transmitted, device_slots, "exactly one ring's worth");
    assert_eq!(rings.tx.len(), Ok(4), "the rest stay queued");

    // Once the device drains four descriptors, the remaining frames flow.
    device.regs.set_tx_consumed(4);
    let report = device.service(&mut rings).expect("service");
    assert_eq!(report.transmitted, 4);
    assert_eq!(rings.tx.len(), Ok(0));
}

#[test]
fn runt_and_oversize_frames_are_dropped_without_wedging_the_queue() {
    let mut device = open();
    let mut region = alloc::vec![0u8; geometry().region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geometry(), BufferClass::NonSensitive).expect("bind");
        rings.tx.push(&[0x01; 4]).expect("queue runt");
        rings.tx.push(&[0x02; 60]).expect("queue good");
    }
    let report = service(&mut device, &mut region, BufferClass::NonSensitive);
    // The runt was consumed and dropped; the good frame behind it flowed.
    assert_eq!(report.transmitted, 1);
    let (start, _) = Genet::<MockRegs, MockDelay>::tx_frame_range(layout(), 0);
    assert_eq!(device.frames.as_bytes()[start], 0x02);
}

#[test]
fn a_device_claiming_impossible_ring_progress_fails_closed() {
    // Nothing was queued, so a transmit consumer index that has moved is a
    // corrupt report: honouring it would free slots still in flight and drive
    // the producer past the ring.
    let mut device = open();
    device.regs.set_tx_consumed(7);
    let mut region = alloc::vec![0u8; geometry().region_len()];
    let mut rings =
        FrameRings::bind(&mut region, geometry(), BufferClass::NonSensitive).expect("bind");
    assert_eq!(
        device.service(&mut rings).err(),
        Some(DriverError::DeviceFault)
    );

    // Likewise a receive producer claiming more completed descriptors than
    // the ring holds: the ring cannot have produced them, so the driver
    // refuses rather than walking descriptor RAM it never armed.
    let mut device = open();
    device.regs.set_rx_produced(layout().ring_slots() + 1);
    assert_eq!(
        device.service(&mut rings).err(),
        Some(DriverError::DeviceFault)
    );

    // Exactly a full ring is legitimate, not a fault.
    let mut device = open();
    device.regs.set_rx_produced(layout().ring_slots());
    assert!(device.service(&mut rings).is_ok());
}

#[test]
fn a_frame_too_large_for_a_device_buffer_is_dropped_not_wedged() {
    let mut device = open();
    // A ring whose slots are wider than this device's frame buffers: a frame
    // filling one cannot be popped into a buffer, and a refused pop leaves
    // the slot occupied, so the driver must release it explicitly or the
    // queue behind it never moves.
    let oversize = BUF_LEN + 512;
    let geometry = RingGeometry::new(4, 4, oversize, oversize, 1).expect("geometry");
    let mut region = alloc::vec![0u8; geometry.region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geometry, BufferClass::NonSensitive).expect("bind");
        rings
            .tx
            .push(&alloc::vec![0x9Cu8; oversize as usize])
            .expect("queue oversize");
        rings.tx.push(&[0x3D; 80]).expect("queue good");
    }
    let mut rings =
        FrameRings::bind(&mut region, geometry, BufferClass::NonSensitive).expect("bind");
    let report = device.service(&mut rings).expect("service");
    // The oversize frame carried no segmentation descriptor, so it was
    // dropped, and the one behind it flowed.
    assert_eq!(report.transmitted, 1);
    assert_eq!(rings.tx.len(), Ok(0), "the queue drained, nothing stuck");
    let (start, _) = Genet::<MockRegs, MockDelay>::tx_frame_range(layout(), 0);
    assert_eq!(device.frames.as_bytes()[start], 0x3D);
}

#[test]
fn a_completed_receive_descriptor_is_delivered_past_the_alignment_pad() {
    let mut device = open();
    // The device wrote a 68-byte frame two bytes into slot 0's buffer.
    let (start, _) = Genet::<MockRegs, MockDelay>::rx_buffer_range(0);
    let from = start + regs::RX_FRAME_OFFSET as usize;
    device.frames.as_bytes_mut()[from..from + 68].fill(0xC3);
    let reported = 68 + regs::RX_FRAME_OFFSET;
    device.regs.set_rx_desc(
        0,
        (reported << regs::DMA_BUFLENGTH_SHIFT) | regs::DMA_SOP | regs::DMA_EOP,
    );
    device.regs.set_rx_produced(1);

    let mut region = alloc::vec![0u8; geometry().region_len()];
    let report = service(&mut device, &mut region, BufferClass::NonSensitive);
    assert_eq!(report.received, 1);
    assert!(!report.rx_ring_full);

    let mut rings =
        FrameRings::bind(&mut region, geometry(), BufferClass::NonSensitive).expect("bind");
    let mut out = alloc::vec![0u8; 2048];
    assert_eq!(rings.rx_ring(0).expect("rx0").pop(&mut out), Ok(Some(68)));
    assert_eq!(&out[..68], &[0xC3; 68]);
    // The slot was handed back to the device.
    let ring = regs::ring_regs(regs::RDMA_DESC, regs::DEFAULT_RING);
    assert_eq!(
        device.regs.writes_to(ring + regs::RING_DRIVER_INDEX).last(),
        Some(&1)
    );
}

#[test]
fn flagged_fragmented_and_malformed_receives_are_dropped() {
    let good = regs::DMA_SOP | regs::DMA_EOP;
    for (label, status) in [
        (
            "crc error",
            (70 << regs::DMA_BUFLENGTH_SHIFT) | good | regs::DMA_RX_CRC_ERROR,
        ),
        (
            "overflow",
            (70 << regs::DMA_BUFLENGTH_SHIFT) | good | regs::DMA_RX_OV,
        ),
        (
            "too long",
            (70 << regs::DMA_BUFLENGTH_SHIFT) | good | regs::DMA_RX_LG,
        ),
        (
            "mac error",
            (70 << regs::DMA_BUFLENGTH_SHIFT) | good | regs::DMA_RX_RXER,
        ),
        (
            "non-octet",
            (70 << regs::DMA_BUFLENGTH_SHIFT) | good | regs::DMA_RX_NO,
        ),
        (
            "fragment (no end)",
            (70 << regs::DMA_BUFLENGTH_SHIFT) | regs::DMA_SOP,
        ),
        (
            "fragment (no start)",
            (70 << regs::DMA_BUFLENGTH_SHIFT) | regs::DMA_EOP,
        ),
        (
            "pad only",
            (regs::RX_FRAME_OFFSET << regs::DMA_BUFLENGTH_SHIFT) | good,
        ),
        ("runt", (10 << regs::DMA_BUFLENGTH_SHIFT) | good),
        (
            "past the buffer",
            ((BUF_LEN + 1) << regs::DMA_BUFLENGTH_SHIFT) | good,
        ),
    ] {
        let mut device = open();
        device.regs.set_rx_desc(0, status);
        device.regs.set_rx_produced(1);
        let mut region = alloc::vec![0u8; geometry().region_len()];
        let report = service(&mut device, &mut region, BufferClass::NonSensitive);
        assert_eq!(report.received, 0, "{label} must not be delivered");
        // The slot is still freed: a bad frame must not wedge the ring.
        let ring = regs::ring_regs(regs::RDMA_DESC, regs::DEFAULT_RING);
        assert_eq!(
            device.regs.writes_to(ring + regs::RING_DRIVER_INDEX).last(),
            Some(&1),
            "{label} must still free its slot"
        );
    }
}

#[test]
fn a_full_receive_ring_back_pressures_without_loss() {
    let mut device = open();
    // Five completed frames through a four-slot receive ring.
    let geometry = RingGeometry::new(
        4,
        4,
        MTU + ETHERNET_HEADER_LEN,
        MTU + ETHERNET_HEADER_LEN,
        1,
    )
    .expect("geometry");
    let reported = 60 + regs::RX_FRAME_OFFSET;
    for slot in 0..5u32 {
        let (start, _) = Genet::<MockRegs, MockDelay>::rx_buffer_range(slot);
        let from = start + regs::RX_FRAME_OFFSET as usize;
        device.frames.as_bytes_mut()[from..from + 60].fill(u8::try_from(slot).unwrap());
        device.regs.set_rx_desc(
            slot,
            (reported << regs::DMA_BUFLENGTH_SHIFT) | regs::DMA_SOP | regs::DMA_EOP,
        );
    }
    device.regs.set_rx_produced(5);

    let mut region = alloc::vec![0u8; geometry.region_len()];
    let mut rings =
        FrameRings::bind(&mut region, geometry, BufferClass::NonSensitive).expect("bind");
    let report = device.service(&mut rings).expect("service");
    assert_eq!(report.received, 4);
    assert!(
        report.rx_ring_full,
        "back-pressure reported, nothing dropped"
    );

    // Drain the ring and pump again: the fifth frame arrives intact.
    let mut out = alloc::vec![0u8; 2048];
    for expected in 0..4u8 {
        assert_eq!(rings.rx_ring(0).expect("rx0").pop(&mut out), Ok(Some(60)));
        assert_eq!(out[0], expected);
    }
    let report = device.service(&mut rings).expect("service");
    assert_eq!(report.received, 1);
    assert_eq!(rings.rx_ring(0).expect("rx0").pop(&mut out), Ok(Some(60)));
    assert_eq!(out[0], 4);
}

#[test]
fn both_rings_wrap_at_their_last_slot() {
    let mut device = open();
    let mut region = alloc::vec![0u8; geometry().region_len()];
    // Walk the transmit ring right round: each pass fills one slot, and the
    // device is told it consumed it, so the ring never fills.
    for pass in 0..layout().ring_slots() + 3 {
        {
            let mut rings =
                FrameRings::bind(&mut region, geometry(), BufferClass::NonSensitive).expect("bind");
            rings.tx.push(&[0x77; 64]).expect("queue");
        }
        let report = service(&mut device, &mut region, BufferClass::NonSensitive);
        assert_eq!(report.transmitted, 1, "pass {pass}");
        device.regs.set_tx_consumed(pass + 1);
    }
    // The producer index wrapped past the ring and the slot index came back
    // round with it.
    assert_eq!(device.tx_producer, layout().ring_slots() + 3);
    assert_eq!(device.tx_producer % layout().ring_slots(), 3);
}

#[test]
fn a_sensitive_ring_scrubs_both_directions_staging() {
    let mut device = open();
    let mut region = alloc::vec![0u8; geometry().region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geometry(), BufferClass::Sensitive).expect("bind");
        rings.tx.push(&[0xEE; 128]).expect("queue");
    }
    service(&mut device, &mut region, BufferClass::Sensitive);
    let (tx_start, tx_end) = Genet::<MockRegs, MockDelay>::tx_buffer_range(layout(), 0);
    // The frame is still staged while the device owns it.
    assert!(device.frames.as_bytes()[tx_start..tx_end].contains(&0xEE));
    // Once the device has consumed it, the reclaim scrubs the buffer.
    device.regs.set_tx_consumed(1);
    service(&mut device, &mut region, BufferClass::Sensitive);
    assert!(device.frames.as_bytes()[tx_start..tx_end]
        .iter()
        .all(|&b| b == 0));

    // A delivered receive buffer is scrubbed before its slot is handed back.
    let (rx_start, rx_end) = Genet::<MockRegs, MockDelay>::rx_buffer_range(0);
    let from = rx_start + regs::RX_FRAME_OFFSET as usize;
    device.frames.as_bytes_mut()[from..from + 60].fill(0xDD);
    let reported = 60 + regs::RX_FRAME_OFFSET;
    device.regs.set_rx_desc(
        0,
        (reported << regs::DMA_BUFLENGTH_SHIFT) | regs::DMA_SOP | regs::DMA_EOP,
    );
    device.regs.set_rx_produced(1);
    let report = service(&mut device, &mut region, BufferClass::Sensitive);
    assert_eq!(report.received, 1);
    assert!(device.frames.as_bytes()[rx_start..rx_end]
        .iter()
        .all(|&b| b == 0));
}

#[test]
fn a_non_sensitive_ring_leaves_its_staging_alone() {
    let mut device = open();
    let mut region = alloc::vec![0u8; geometry().region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geometry(), BufferClass::NonSensitive).expect("bind");
        rings.tx.push(&[0xEE; 128]).expect("queue");
    }
    service(&mut device, &mut region, BufferClass::NonSensitive);
    device.regs.set_tx_consumed(1);
    service(&mut device, &mut region, BufferClass::NonSensitive);
    let (start, end) = Genet::<MockRegs, MockDelay>::tx_buffer_range(layout(), 0);
    assert!(device.frames.as_bytes()[start..end].contains(&0xEE));
}

// --- the load gate ------------------------------------------------------

#[test]
fn register_requires_the_driver_load_capability() {
    struct Host {
        granted: bool,
    }
    impl DriverHost for Host {
        fn has_capability(&self, id: CapabilityId) -> bool {
            self.granted && id == CapabilityId::DRV_LOAD
        }

        fn kind(&self) -> tairix_abi::DriverKind {
            tairix_abi::DriverKind::UserSpace
        }
    }
    assert!(register(&Host { granted: true }).is_ok());
    assert_eq!(
        register(&Host { granted: false }).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn the_bind_table_matches_only_a_genet_v5_node() {
    assert_eq!(BIND_KEYS.len(), 1);
    assert_eq!(BIND_KEYS[0].priority, BIND_PRIORITY);
    let genet = HwMatchKey::compatible(GENET_COMPATIBLE).expect("fits");
    assert!(BIND_KEYS[0].key.matches(&genet));
    // A different Broadcom Ethernet revision, and an unrelated node, both
    // fail the match rather than binding this register layout.
    let v4 = HwMatchKey::compatible(b"brcm,genet-v4").expect("fits");
    assert!(!BIND_KEYS[0].key.matches(&v4));
    assert!(!BIND_KEYS[0].key.matches(&HwMatchKey::virtio(1)));
}

// --- offloads -----------------------------------------------------------

/// The ring geometry the transport offers once segmentation is negotiated:
/// transmit slots sized for a whole super-frame.
fn tso_geometry() -> RingGeometry {
    let facts = DeviceFacts {
        mac: MacAddress::new(MAC),
        mtu: MTU,
        link: LinkState::Up,
        offloads: OFFLOADS,
        rx_queues: 1,
        max_tx_frame: layout().tx_staging(),
        multicast_filter: McastFilter::Slots(MCAST_SLOTS),
    };
    RingGeometry::for_device(&facts, Some(&test_machine())).expect("valid geometry")
}

/// The transmit status block's directive word for slot `slot`.
fn tsb_directive(device: &Genet<MockRegs, MockDelay>, slot: u32) -> u32 {
    let (start, _) = Genet::<MockRegs, MockDelay>::tx_buffer_range(layout(), slot);
    let block = &device.frames.as_bytes()[start..start + regs::TSB_LEN as usize];
    u32::from_le_bytes([
        block[regs::TSB_CSUM_INFO],
        block[regs::TSB_CSUM_INFO + 1],
        block[regs::TSB_CSUM_INFO + 2],
        block[regs::TSB_CSUM_INFO + 3],
    ])
}

/// The descriptor status word for transmit slot `slot`.
fn tx_status(device: &Genet<MockRegs, MockDelay>, slot: u32) -> u32 {
    device
        .regs
        .peek(regs::desc(regs::TDMA_DESC, slot) + regs::DESC_LENGTH_STATUS)
}

/// One Ethernet + IPv4 + TCP frame carrying `payload` bytes, of the shape
/// the stack emits for a checksum or segmentation offload.
fn tcp_frame(protocol: u8, payload: usize) -> Vec<u8> {
    let mut frame = alloc::vec![0u8; 14 + 20 + 20 + payload];
    frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    frame[14] = 0x45;
    let total = u16::try_from(20 + 20 + payload).expect("bounded");
    frame[16..18].copy_from_slice(&total.to_be_bytes());
    frame[18..20].copy_from_slice(&0x1234u16.to_be_bytes());
    frame[22] = 64;
    frame[23] = protocol;
    frame[26..30].copy_from_slice(&[10, 0, 2, 15]);
    frame[30..34].copy_from_slice(&[10, 0, 2, 2]);
    frame[34..36].copy_from_slice(&1234u16.to_be_bytes());
    frame[36..38].copy_from_slice(&80u16.to_be_bytes());
    frame[38..42].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    frame[46] = 5 << 4;
    frame[47] = 0x18; // ACK | PSH
    frame
}

#[test]
fn bring_up_arms_both_checksum_engines() {
    let device = open();
    assert_eq!(
        device.regs.peek(regs::RBUF_CHK_CTRL) & regs::RBUF_RXCHK_EN,
        regs::RBUF_RXCHK_EN,
        "the receive checksum engine is enabled"
    );
    assert_eq!(
        device.regs.peek(regs::RBUF_CHK_CTRL) & regs::RBUF_SKIP_FCS,
        0,
        "the MAC strips the frame check sequence, so the engine must not skip it"
    );
    assert_eq!(
        device.regs.peek(regs::TBUF_CTRL) & regs::TBUF_64B_EN,
        regs::TBUF_64B_EN,
        "the transmit status block carries the checksum directive"
    );
    // Bit 0 of `RBUF_CTRL` is the receive status block, which would move
    // every frame 64 octets further into its buffer. It stays off.
    assert_eq!(device.regs.peek(regs::RBUF_CTRL) & 1, 0);
}

#[test]
fn a_verified_receive_checksum_reaches_the_stack_as_validated() {
    let mut device = open();
    let (start, _) = Genet::<MockRegs, MockDelay>::rx_buffer_range(0);
    let from = start + regs::RX_FRAME_OFFSET as usize;
    device.frames.as_bytes_mut()[from..from + 68].fill(0xC3);
    let reported = 68 + regs::RX_FRAME_OFFSET;
    device.regs.set_rx_desc(
        0,
        (reported << regs::DMA_BUFLENGTH_SHIFT)
            | regs::DMA_SOP
            | regs::DMA_EOP
            | regs::DMA_RX_CHK_OK,
    );
    device.regs.set_rx_produced(1);
    let mut region = alloc::vec![0u8; geometry().region_len()];
    let report = service(&mut device, &mut region, BufferClass::NonSensitive);
    assert_eq!(report.received, 1);

    let mut rings =
        FrameRings::bind(&mut region, geometry(), BufferClass::NonSensitive).expect("bind");
    let mut offload = FrameOffload::None;
    let mut out = alloc::vec![0u8; 2048];
    rings
        .rx_ring(0)
        .expect("rx0")
        .pop_with(&mut offload, &mut out)
        .expect("pop")
        .expect("a frame");
    assert_eq!(offload, FrameOffload::Validated);
}

#[test]
fn an_unparsed_receive_frame_keeps_the_software_fold() {
    let mut device = open();
    let (start, _) = Genet::<MockRegs, MockDelay>::rx_buffer_range(0);
    let from = start + regs::RX_FRAME_OFFSET as usize;
    device.frames.as_bytes_mut()[from..from + 68].fill(0xC3);
    let reported = 68 + regs::RX_FRAME_OFFSET;
    device.regs.set_rx_desc(
        0,
        (reported << regs::DMA_BUFLENGTH_SHIFT) | regs::DMA_SOP | regs::DMA_EOP,
    );
    device.regs.set_rx_produced(1);
    let mut region = alloc::vec![0u8; geometry().region_len()];
    service(&mut device, &mut region, BufferClass::NonSensitive);

    let mut rings =
        FrameRings::bind(&mut region, geometry(), BufferClass::NonSensitive).expect("bind");
    let mut offload = FrameOffload::None;
    let mut out = alloc::vec![0u8; 2048];
    rings
        .rx_ring(0)
        .expect("rx0")
        .pop_with(&mut offload, &mut out)
        .expect("pop");
    assert_eq!(offload, FrameOffload::None);
}

#[test]
fn a_partial_checksum_frame_directs_the_transmit_engine() {
    let mut device = open();
    let frame = tcp_frame(6, 100);
    let mut region = alloc::vec![0u8; geometry().region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geometry(), BufferClass::NonSensitive).expect("bind");
        rings
            .tx
            .push_with(
                FrameOffload::TxChecksum {
                    csum_start: 34,
                    csum_offset: 16,
                },
                &frame,
            )
            .expect("queue");
    }
    let report = service(&mut device, &mut region, BufferClass::NonSensitive);
    assert_eq!(report.transmitted, 1);

    let directive = tsb_directive(&device, 0);
    assert_eq!(directive & regs::TSB_CSUM_LV, regs::TSB_CSUM_LV);
    assert_eq!(
        (directive >> regs::TSB_CSUM_START_SHIFT) & regs::TSB_CSUM_OFFSET_MASK,
        34,
        "the fold starts at the transport header, the status block excluded"
    );
    assert_eq!(directive & regs::TSB_CSUM_OFFSET_MASK, 50);
    assert_eq!(
        directive & regs::TSB_CSUM_PROTO_UDP,
        0,
        "TCP takes no RFC 768 zero-checksum rule"
    );
    assert_eq!(
        tx_status(&device, 0) & regs::DMA_TX_DO_CSUM,
        regs::DMA_TX_DO_CSUM
    );
}

#[test]
fn a_udp_frame_is_flagged_so_a_zero_checksum_is_sent_as_all_ones() {
    let mut device = open();
    let frame = tcp_frame(17, 40);
    let mut region = alloc::vec![0u8; geometry().region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geometry(), BufferClass::NonSensitive).expect("bind");
        rings
            .tx
            .push_with(
                FrameOffload::TxChecksum {
                    csum_start: 34,
                    csum_offset: 6,
                },
                &frame,
            )
            .expect("queue");
    }
    service(&mut device, &mut region, BufferClass::NonSensitive);
    assert_eq!(
        tsb_directive(&device, 0) & regs::TSB_CSUM_PROTO_UDP,
        regs::TSB_CSUM_PROTO_UDP
    );
}

#[test]
fn a_checksum_offset_past_the_frame_drops_it_rather_than_sending_a_partial() {
    let mut device = open();
    let frame = tcp_frame(6, 0);
    let mut region = alloc::vec![0u8; geometry().region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geometry(), BufferClass::NonSensitive).expect("bind");
        rings
            .tx
            .push_with(
                FrameOffload::TxChecksum {
                    csum_start: 34,
                    // Past the end of a 54-byte frame.
                    csum_offset: 400,
                },
                &frame,
            )
            .expect("queue");
        rings.tx.push(&[0x5E; 64]).expect("queue good");
    }
    let report = service(&mut device, &mut region, BufferClass::NonSensitive);
    assert_eq!(
        report.transmitted, 1,
        "the unhonourable frame is dropped, never sent half-checksummed"
    );
    let (start, _) = Genet::<MockRegs, MockDelay>::tx_frame_range(layout(), 0);
    assert_eq!(device.frames.as_bytes()[start], 0x5E);
}

#[test]
fn a_super_frame_is_split_into_wire_frames_the_engine_checksums() {
    let mut device = open();
    let geometry = tso_geometry();
    // Three segments at an MSS of 1000.
    let frame = tcp_frame(6, 2_500);
    let mut region = alloc::vec![0u8; geometry.region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geometry, BufferClass::NonSensitive).expect("bind");
        rings
            .tx
            .push_with(
                FrameOffload::TxSegment {
                    csum_start: 34,
                    csum_offset: 16,
                    gso_size: 1_000,
                    hdr_len: 54,
                    ipv6: false,
                },
                &frame,
            )
            .expect("queue");
    }
    let mut rings =
        FrameRings::bind(&mut region, geometry, BufferClass::NonSensitive).expect("bind");
    let report = device.service(&mut rings).expect("service");
    assert_eq!(
        report.transmitted, 3,
        "one ring slot became three wire frames"
    );

    let sizes = [1_000u32, 1_000, 500];
    for (slot, payload) in sizes.iter().enumerate() {
        let slot = u32::try_from(slot).expect("bounded");
        let status = tx_status(&device, slot);
        assert_eq!(
            (status >> regs::DMA_BUFLENGTH_SHIFT) & regs::DMA_BUFLENGTH_MASK,
            regs::TSB_LEN + 54 + payload
        );
        assert_eq!(
            status & (regs::DMA_SOP | regs::DMA_EOP),
            regs::DMA_SOP | regs::DMA_EOP,
            "each segment is a whole frame of its own"
        );
        assert_eq!(
            status & regs::DMA_TX_DO_CSUM,
            regs::DMA_TX_DO_CSUM,
            "every segment's checksum is completed by the engine"
        );
        assert_eq!(
            tsb_directive(&device, slot) & regs::TSB_CSUM_LV,
            regs::TSB_CSUM_LV
        );
    }
    // The payload ran in order across the three slots.
    let mut seen = 0usize;
    for (slot, payload) in sizes.iter().enumerate() {
        let (start, _) = Genet::<MockRegs, MockDelay>::tx_frame_range(
            layout(),
            u32::try_from(slot).expect("bounded"),
        );
        let bytes = &device.frames.as_bytes()[start + 54..start + 54 + *payload as usize];
        assert_eq!(bytes, &frame[54 + seen..54 + seen + *payload as usize]);
        seen += *payload as usize;
    }
    assert_eq!(seen, 2_500);
}

#[test]
fn a_full_transmit_ring_defers_the_rest_of_a_split_rather_than_dropping_it() {
    let mut device = open();
    let geometry = tso_geometry();
    // More segments than the device has descriptors, so the split cannot
    // finish in one doorbell.
    let payload = (layout().ring_slots() as usize + 3) * 100;
    let frame = tcp_frame(6, payload);
    let mut region = alloc::vec![0u8; geometry.region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geometry, BufferClass::NonSensitive).expect("bind");
        rings
            .tx
            .push_with(
                FrameOffload::TxSegment {
                    csum_start: 34,
                    csum_offset: 16,
                    gso_size: 100,
                    hdr_len: 54,
                    ipv6: false,
                },
                &frame,
            )
            .expect("queue");
    }
    let mut rings =
        FrameRings::bind(&mut region, geometry, BufferClass::NonSensitive).expect("bind");
    let first = device.service(&mut rings).expect("service");
    assert_eq!(first.transmitted, layout().ring_slots(), "the ring filled");
    assert!(
        device.staged.is_some(),
        "the remainder is kept, not dropped"
    );

    // The device drains its ring; the next doorbell finishes the split.
    device.regs.set_tx_consumed(layout().ring_slots());
    let second = device.service(&mut rings).expect("service");
    assert_eq!(second.transmitted, 3);
    assert!(device.staged.is_none(), "the super-frame is complete");
}

#[test]
fn a_segmentation_descriptor_the_frame_does_not_bear_out_is_dropped() {
    let mut device = open();
    let geometry = tso_geometry();
    let frame = tcp_frame(6, 2_000);
    let mut region = alloc::vec![0u8; geometry.region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geometry, BufferClass::NonSensitive).expect("bind");
        rings
            .tx
            .push_with(
                FrameOffload::TxSegment {
                    csum_start: 34,
                    csum_offset: 16,
                    gso_size: 1_000,
                    // Claims a 40-octet TCP header the frame's data offset
                    // does not agree with.
                    hdr_len: 74,
                    ipv6: false,
                },
                &frame,
            )
            .expect("queue");
        rings.tx.push(&[0x77; 64]).expect("queue good");
    }
    let mut rings =
        FrameRings::bind(&mut region, geometry, BufferClass::NonSensitive).expect("bind");
    let report = device.service(&mut rings).expect("service");
    assert_eq!(report.transmitted, 1, "only the well-formed frame flowed");
    assert!(device.staged.is_none());
    let (start, _) = Genet::<MockRegs, MockDelay>::tx_frame_range(layout(), 0);
    assert_eq!(device.frames.as_bytes()[start], 0x77);
}

#[test]
fn a_sensitive_ring_scrubs_the_staged_super_frame_after_the_split() {
    let mut device = open();
    let geometry = tso_geometry();
    let frame = tcp_frame(6, 1_500);
    let mut region = alloc::vec![0u8; geometry.region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geometry, BufferClass::Sensitive).expect("bind");
        rings
            .tx
            .push_with(
                FrameOffload::TxSegment {
                    csum_start: 34,
                    csum_offset: 16,
                    gso_size: 1_000,
                    hdr_len: 54,
                    ipv6: false,
                },
                &frame,
            )
            .expect("queue");
    }
    let mut rings = FrameRings::bind(&mut region, geometry, BufferClass::Sensitive).expect("bind");
    device.service(&mut rings).expect("service");
    let staged = &device.frames.as_bytes()[buffer_bytes()..buffer_bytes() + frame.len()];
    assert!(
        staged.iter().all(|&b| b == 0),
        "the staged plaintext must not outlive its transmission"
    );
}

#[test]
fn a_segment_size_past_a_transmit_buffer_is_refused_rather_than_wedging() {
    let mut device = open();
    let geometry = tso_geometry();
    let frame = tcp_frame(6, 8_000);
    let mut region = alloc::vec![0u8; geometry.region_len()];
    {
        let mut rings =
            FrameRings::bind(&mut region, geometry, BufferClass::NonSensitive).expect("bind");
        rings
            .tx
            .push_with(
                FrameOffload::TxSegment {
                    csum_start: 34,
                    csum_offset: 16,
                    // Larger than any frame the MAC will send.
                    gso_size: 4_000,
                    hdr_len: 54,
                    ipv6: false,
                },
                &frame,
            )
            .expect("queue");
        rings.tx.push(&[0x2A; 64]).expect("queue good");
    }
    let mut rings =
        FrameRings::bind(&mut region, geometry, BufferClass::NonSensitive).expect("bind");
    let report = device.service(&mut rings).expect("service");
    assert_eq!(report.transmitted, 1);
    assert!(device.staged.is_none());
    let (start, _) = Genet::<MockRegs, MockDelay>::tx_frame_range(layout(), 0);
    assert_eq!(device.frames.as_bytes()[start], 0x2A);
}
