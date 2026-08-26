//! TAIRiX Broadcom GENET v5 link-layer network driver (Raspberry Pi 4B
//! on-board gigabit Ethernet).
//!
//! The Pi 4's `brcm,bcm2711-genet-v5` MAC is a DMA-mastering gigabit
//! controller with an embedded MDIO master driving an external RGMII PHY
//! (a BCM54213PE). This driver brings it up over the single register
//! aperture the discovered hardware-tree node names and serves frames
//! through [`tairix_abi::driver::net::Net`], so the network stack drives it
//! exactly as it drives any other NIC.
//!
//! # Layered seams
//!
//! The device logic is written against two injected seams, so the whole
//! bring-up and frame path is proven host-side against a register-level
//! model of the controller:
//!
//! * [`GenetRegs`] — register access (a capability-gated
//!   [`RegisterWindow`] on metal, a mock controller in tests). Both methods
//!   take `&mut self` so a model can represent registers with read side
//!   effects (the MDIO busy bit, the DMA producer indices).
//! * [`Delay`] — microsecond delay and monotonic time, so every hardware
//!   handshake is bounded by wall clock and the CPU sleeps between polls
//!   rather than spinning.
//!
//! QEMU models no GENET, so there is no emulated vertical for this device;
//! the register-level suite in `tests.rs` is the coverage and the live path
//! is an on-metal acceptance item (`plans/PI.md`).
//!
//! # Frame path
//!
//! Descriptors live in the controller's own on-chip RAM inside the register
//! aperture, so the only DMA is the frame buffers: one
//! [`DmaLayout`]-sized carve holds one receive and one transmit buffer of
//! [`BUF_LEN`] bytes per programmed descriptor, allocated **once**
//! at [`Genet::open`] and reused for every frame — no per-packet carve, no
//! allocator on the hot path.
//!
//! [`Net::service`] is a non-blocking doorbell: it reclaims completed
//! transmits, drains the ring's queued frames into free transmit slots, and
//! harvests delivered frames, then returns. It never waits for the device,
//! so it is safe to serve across the process boundary the network stack
//! calls over. A full receive ring is reported as back-pressure
//! ([`ServiceReport::rx_ring_full`]) with the frame left in its descriptor —
//! nothing is dropped behind the stack's back.
//!
//! # Offloads
//!
//! **Receive checksum.** `RBUF_RXCHK_EN` puts the checksum engine in front of
//! the receiver; it parses the frame's L3/L4 headers and reports its verdict
//! in bit 15 of the completed descriptor. The frame itself is untouched, so
//! the 64-byte receive status block (`RBUF_64B_EN` in the reference driver)
//! is deliberately **not** enabled and the receive-buffer layout is
//! exactly what it was without the offload. A frame the engine verified is
//! delivered as [`FrameOffload::Validated`]; anything it did not parse simply
//! keeps the stack's software fold.
//!
//! **Transmit checksum.** `TBUF_64B_EN` prefixes every transmitted buffer
//! with a [`regs::TSB_LEN`]-byte status block whose one live word directs the
//! engine at the transport checksum the stack left partial. The block is
//! consumed by the controller and never reaches the wire.
//!
//! **Segmentation.** The GENET has no segmentation engine, so the driver
//! splits a [`FrameOffload::TxSegment`] super-frame itself through the shared
//! [`TcpSegmenter`] and hands each wire segment to the
//! transmit checksum engine — the same shape Linux's `net/core/tso.c` gives
//! `mvneta` and `fec`. The win is the offload's real one: one ring slot and
//! one stack transmit pass for tens of packets. A super-frame the ring cannot
//! absorb in one doorbell stays staged and resumes at the next, so a full
//! ring defers segments rather than dropping them.
//!
//! # Public surface
//!
//! The only public *function* is [`register`]. [`Genet`] is a public type
//! the driver process instantiates through [`wiring::open_discovered`]; the
//! process never reaches into it beyond the [`Net`] trait.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]; mapping the discovered
//! register window additionally requires [`CapabilityId::MMIO_MAP`] and
//! carving the frame buffers [`CapabilityId::MEM_DMA`] (both checked in
//! [`wiring`]). The driver runs in user space and does not request
//! `CAP_DRV_KERNEL`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::driver::dma::DmaSlab;
use tairix_abi::driver::mmio::WindowError;
use tairix_abi::driver::net::{
    frame_capacity, DeviceFacts, LinkState, MacAddress, McastFilter, Net, NetOffloads,
    ETHERNET_HEADER_LEN,
};
use tairix_abi::driver::net_ring::{
    FrameOffload, FrameRings, RingBudget, RingGeometry, RxDelivery, ServiceReport,
};
use tairix_abi::driver::timing::Delay;
use tairix_abi::{
    BootFacts, CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, Errno,
    HwMatchKey, RegisterWindow,
};

pub mod mdio;
pub mod regs;
pub mod wiring;

#[cfg(test)]
mod tests;

use tairix_net::txoffload::{transport_protocol, TcpSegmenter};
use tairix_net::udp::PROTOCOL_UDP;

use mdio::{Link, PHY_ADDRESS};

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// The bytes spell `"GNT"` (GENET) with a version nibble, matching the other
/// drivers' marker convention.
const REGISTER_HANDLE_MARKER: u64 = 0x474E_5400_0000_0001;

/// The bind priority [`BIND_KEYS`] carries.
///
/// An exact `compatible`-string match: it ranks at the exact-match tier
/// (higher matched priority binds; an unbroken tie is a packaging defect).
const BIND_PRIORITY: u16 = 10;

/// Device-tree `compatible` string of the BCM2711's GENET v5 MAC — the
/// identity a discovered node must advertise for this driver to bind it.
pub const GENET_COMPATIBLE: &[u8] = b"brcm,bcm2711-genet-v5";

/// CPU-physical base of the BCM2711's GENET register aperture.
///
/// The board fact a `network.conf` `<iface>.match.node` key names to bind an
/// interface alias to this NIC by where it sits on the bus, so the shipped
/// Raspberry Pi image's addressing default and the location the device
/// manager resolves from the discovered node cannot drift. The **driver**
/// never uses it: it maps only the window its matched node named.
pub const GENET_REGS_CPU_BASE: u64 = 0xFD58_0000;

/// This driver's hardware bind table: the BCM2711 GENET v5 MAC, matched by
/// its device-tree `compatible` string ([`GENET_COMPATIBLE`]).
///
/// The single source of truth the signed-manifest bind table is authored
/// from and a discovered node is resolved against.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    match HwMatchKey::compatible(GENET_COMPATIBLE) {
        Ok(key) => key,
        // Unreachable: the literal is well within `HW_COMPATIBLE_MAX`. A
        // too-long literal would be a compile-time const-eval error here,
        // never a runtime panic.
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    },
)];

/// Bytes per frame buffer. The controller's receive buffer size must cover
/// the largest frame plus the two-byte alignment pad it inserts; 2 KiB is
/// the natural power-of-two above that and keeps each buffer on its own
/// cache-line-aligned stride.
pub const BUF_LEN: u32 = 2048;

/// How the driver's one DMA carve is laid out: the descriptors it programs
/// each ring to use, and the segmentation staging area behind them.
///
/// Both figures are derived from the discovered machine through the shared
/// ring-sizing policy, never hand-picked: the same policy the network stack
/// sizes the shared frame rings with, so a small board and a server each get
/// a coherent set of depths and the two sides cannot disagree about what the
/// other can afford. The descriptor count is additionally bounded by the
/// controller's own descriptor RAM ([`regs::TOTAL_DESC`] per direction).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DmaLayout {
    ring_slots: u32,
    tx_staging: u32,
}

impl DmaLayout {
    /// Derive the layout for `machine`.
    #[must_use]
    pub fn for_machine(machine: Option<&BootFacts>) -> Self {
        let budget = RingBudget::for_machine(machine);
        // The link MTU is fixed for this controller, so the floor cannot
        // overflow; a floor of one whole frame is the honest fallback.
        let floor = frame_capacity(MTU).unwrap_or(ETHERNET_HEADER_LEN);
        Self {
            ring_slots: budget.slots(BUF_LEN as usize, regs::TOTAL_DESC),
            tx_staging: budget.slot_capacity(floor, RingGeometry::MAX_SLOT_CAPACITY),
        }
    }

    /// Descriptors the driver programs each direction's ring to use.
    #[must_use]
    pub const fn ring_slots(self) -> u32 {
        self.ring_slots
    }

    /// The descriptor `counter` addresses. The depth is a power of two, so
    /// the wrap is a mask rather than a division on the frame path.
    #[must_use]
    pub const fn ring_index(self, counter: u32) -> u32 {
        counter & (self.ring_slots - 1)
    }

    /// Largest single transmit frame the staging area holds — the driver's
    /// [`DeviceFacts::max_tx_frame`] report, so the stack never offers a
    /// super-frame slot this carve could not stage.
    #[must_use]
    pub const fn tx_staging(self) -> u32 {
        self.tx_staging
    }

    /// Bytes the frame buffers occupy: one [`BUF_LEN`] buffer per receive
    /// and per transmit descriptor.
    const fn buffer_bytes(self) -> usize {
        2 * (self.ring_slots as usize) * (BUF_LEN as usize)
    }

    /// Bytes of device-visible DMA the driver carves once at
    /// [`Genet::open`]: the frame buffers then the staging area.
    #[must_use]
    pub const fn bytes(self) -> usize {
        self.buffer_bytes() + self.tx_staging as usize
    }
}

/// Destination addresses bring-up admits through the receive filter before
/// any group address: this station's own unicast address and the broadcast
/// address. They occupy the first slots and are never displaced.
const RX_FILTER_ADDRESSES: u16 = 2;

/// Filter slots left for group addresses once the fixed ones are spent.
const MCAST_SLOTS: u16 = regs::MDF_SLOTS - RX_FILTER_ADDRESSES;

const _: () = assert!(RX_FILTER_ADDRESSES < regs::MDF_SLOTS);

/// The link MTU this driver reports: standard Ethernet.
pub const MTU: u32 = 1500;

/// Octets a VLAN tag adds to a frame the receiver must still accept.
const VLAN_TAG_LEN: u32 = 4;

/// Octets a Broadcom switch tag adds ahead of the Ethernet header. The MAC
/// accepts one even though this driver never emits or parses it, so the
/// receiver's length limit must leave room for it.
const BRCM_TAG_LEN: u32 = 6;

/// Octets of frame check sequence.
const FCS_LEN: u32 = 4;

/// Longest frame the receiver accepts, programmed into
/// [`regs::UMAC_MAX_FRAME_LEN`]: an MTU-sized payload plus every header the
/// MAC may see, rounded up to the controller's 8-byte granularity.
const MAX_FRAME_LEN: u32 =
    (MTU + ETHERNET_HEADER_LEN + VLAN_TAG_LEN + BRCM_TAG_LEN + FCS_LEN + 7) & !7;

/// A transmit buffer holds the status block the checksum engine reads and
/// then the longest frame the MAC will send, so neither can ever be split
/// across buffers.
const _: () = assert!(regs::TSB_LEN + MAX_FRAME_LEN <= BUF_LEN);

/// The descriptor's length field must span both.
const _: () = assert!(regs::TSB_LEN + MAX_FRAME_LEN <= regs::DMA_BUFLENGTH_MASK);

/// The offload set this driver serves: the receive checksum verdict, the
/// transmit checksum engine for both transports, and driver-side TCP
/// segmentation over it.
const OFFLOADS: NetOffloads = match NetOffloads::from_bits(
    NetOffloads::RX_CSUM_VALIDATED.bits()
        | NetOffloads::TX_CSUM_TCP.bits()
        | NetOffloads::TX_CSUM_UDP.bits()
        | NetOffloads::TX_SEGMENT_TCP.bits(),
) {
    Ok(set) => set,
    // Unreachable: every bit named above is defined. A bit that were not
    // would be a compile-time const-eval error here, never a runtime panic.
    Err(_) => panic!("every advertised offload bit is defined"),
};

/// Microseconds the reset pulses are held. The GENET reset paths are
/// register-strobe resets that settle in a few controller clocks; the
/// reference drivers hold each for 10 µs.
const RESET_HOLD_US: u32 = 10;

/// Driver entry point.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`].
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}

/// The GENET register-access seam.
///
/// Every controller access the [`Genet`] engine makes goes through this
/// trait, so the bring-up sequence and the frame path are proven host-side
/// against a register-level model. Both methods take `&mut self` so a model
/// can represent registers with read side effects (the MDIO `START_BUSY`
/// handshake, the DMA producer/consumer indices).
pub trait GenetRegs {
    /// Read the 32-bit register at byte `offset` within the aperture.
    ///
    /// # Errors
    ///
    /// [`DriverError::OutOfRange`] if the access falls outside the mapped
    /// window or is misaligned.
    fn read(&mut self, offset: usize) -> Result<u32, DriverError>;

    /// Write `value` to the 32-bit register at byte `offset`.
    ///
    /// # Errors
    ///
    /// As [`read`](Self::read).
    fn write(&mut self, offset: usize, value: u32) -> Result<(), DriverError>;
}

impl GenetRegs for RegisterWindow {
    fn read(&mut self, offset: usize) -> Result<u32, DriverError> {
        self.read_u32(offset).map_err(WindowError::as_driver_error)
    }

    fn write(&mut self, offset: usize, value: u32) -> Result<(), DriverError> {
        self.write_u32(offset, value)
            .map_err(WindowError::as_driver_error)
    }
}

/// The GENET v5 device engine.
///
/// Owns the register aperture, the frame-buffer DMA carve, and the ring
/// bookkeeping, and implements [`Net`] over them.
pub struct Genet<R: GenetRegs, D: Delay> {
    regs: R,
    delay: D,
    /// The one frame-buffer carve: receive buffers first, then transmit.
    frames: DmaSlab,
    /// How that carve is laid out — the descriptor count each ring is
    /// programmed to and the staging area behind the buffers.
    layout: DmaLayout,
    /// The address programmed into the UniMAC, reported in
    /// [`DeviceFacts::mac`].
    mac: MacAddress,
    /// The link last resolved from the PHY, or [`None`] while down.
    link: Option<Link>,
    /// An `INTRL2_0` link event arrived and the link must be re-resolved on
    /// the next service doorbell (the interrupt acknowledgement cannot
    /// report an error, so the MDIO work happens where it can).
    link_event: bool,
    /// Free-running 16-bit counter the driver publishes as the receive
    /// ring's consumer index; its low bits also index the descriptor.
    rx_consumer: u32,
    /// Free-running 16-bit counter the driver publishes as the transmit
    /// ring's producer index.
    tx_producer: u32,
    /// The transmit consumer index last read back from the device, so the
    /// count in flight — and hence the free slots — is known without
    /// re-reading mid-drain.
    tx_consumer: u32,
    /// Frames the receive pre-filter has shed since open. A cumulative
    /// device statistic, so a consumer that misses a report loses nothing.
    filtered_frames: u64,
    /// A segmentation super-frame sitting in the staging area with part of
    /// its payload still to reach the wire, because the transmit ring filled
    /// mid-split. It is finished before any later frame is dequeued, so one
    /// stream's wire order is never disturbed.
    staged: Option<Staged>,
}

/// A staged segmentation super-frame and how far its split has got.
#[derive(Copy, Clone, Debug)]
struct Staged {
    /// Bytes of the super-frame in the staging area.
    length: usize,
    /// The segmentation descriptor the stack attached to it.
    offload: FrameOffload,
    /// Payload bytes already segmented onto the wire.
    emitted: usize,
}

impl<R: GenetRegs, D: Delay> Genet<R, D> {
    /// Bring the controller online: verify it really is a GENET v5, reset
    /// the MAC, program `mac`, build both DMA rings over `frames`, start the
    /// PHY, and enable transmit and receive.
    ///
    /// `layout` is the derived carve layout `frames` was allocated for;
    /// a carve shorter than [`DmaLayout::bytes`] is refused rather than
    /// driving the device against buffers that are not there.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `frames` is undersized.
    /// * [`DriverError::Unsupported`] if the controller does not report the
    ///   GENET v5 core revision — the matched node claimed a device this
    ///   register layout does not describe, so bring-up refuses rather than
    ///   programming a foreign block.
    /// * [`DriverError::OutOfRange`] if a register access falls outside the
    ///   mapped aperture (a short window).
    /// * [`DriverError::DeviceFault`] if the PHY never answers on MDIO.
    pub fn open(
        regs: R,
        delay: D,
        frames: DmaSlab,
        mac: MacAddress,
        layout: DmaLayout,
    ) -> Result<Self, DriverError> {
        if frames.len() < layout.bytes() {
            return Err(DriverError::BufferTooSmall);
        }
        let mut device = Self {
            regs,
            delay,
            frames,
            layout,
            mac,
            link: None,
            link_event: false,
            rx_consumer: 0,
            tx_producer: 0,
            tx_consumer: 0,
            filtered_frames: 0,
            staged: None,
        };
        device.check_revision()?;
        // Mask every level-2 source before touching the device, so a
        // condition left asserted by whatever ran before cannot storm the
        // line while bring-up programs the rings. Both instances: this
        // driver never binds `INTRL2_1`'s line and never services it, so
        // leaving its sources live would leave part of the device's
        // interrupt state to whatever the firmware left behind.
        device
            .regs
            .write(regs::INTRL2_CPU_MASK_SET, regs::INTRL2_ALL)?;
        device
            .regs
            .write(regs::INTRL2_CPU_CLEAR, regs::INTRL2_ALL)?;
        device
            .regs
            .write(regs::INTRL2_1_CPU_MASK_SET, regs::INTRL2_ALL)?;
        device.reset_umac()?;
        device.write_hwaddr()?;
        device.write_rx_filter(&[])?;
        device.disable_dma()?;
        device.init_rx()?;
        device.init_tx()?;
        device.enable_dma()?;
        mdio::start_autoneg(&mut device.regs, &device.delay, PHY_ADDRESS)?;
        device.link = mdio::await_link(&mut device.regs, &device.delay, PHY_ADDRESS)?;
        device.apply_link()?;
        device
            .regs
            .write(regs::INTRL2_CPU_MASK_CLEAR, regs::IRQ_ENABLED)?;
        Ok(device)
    }

    /// Refuse a controller that does not report the GENET v5 core revision.
    fn check_revision(&mut self) -> Result<(), DriverError> {
        let rev = self.regs.read(regs::SYS_REV_CTRL)?;
        let major = (rev >> regs::REV_MAJOR_SHIFT) & regs::REV_MAJOR_MASK;
        if major != regs::GENET_V5_MAJOR {
            return Err(DriverError::Unsupported);
        }
        Ok(())
    }

    /// Reset the receive buffer and the UniMAC, clear the statistics
    /// counters, and program the frame limit and receive-buffer behaviour.
    ///
    /// `CMD_CRC_FWD` is deliberately left clear, so the MAC strips the frame
    /// check sequence and a descriptor's reported length is the frame
    /// length.
    fn reset_umac(&mut self) -> Result<(), DriverError> {
        let flush = self.regs.read(regs::SYS_RBUF_FLUSH_CTRL)?;
        self.regs
            .write(regs::SYS_RBUF_FLUSH_CTRL, flush | regs::RBUF_FLUSH_RESET)?;
        self.delay.delay_us(RESET_HOLD_US);
        self.regs
            .write(regs::SYS_RBUF_FLUSH_CTRL, flush & !regs::RBUF_FLUSH_RESET)?;
        self.delay.delay_us(RESET_HOLD_US);
        self.regs.write(regs::SYS_RBUF_FLUSH_CTRL, 0)?;
        self.delay.delay_us(RESET_HOLD_US);

        // Hold the MAC in local loopback across its software reset, so no
        // partial frame reaches the wire while it is being reset.
        self.regs.write(regs::UMAC_CMD, 0)?;
        self.regs
            .write(regs::UMAC_CMD, regs::CMD_SW_RESET | regs::CMD_LCL_LOOP_EN)?;
        self.delay.delay_us(RESET_HOLD_US);
        self.regs.write(regs::UMAC_CMD, 0)?;

        self.regs.write(
            regs::UMAC_MIB_CTRL,
            regs::MIB_RESET_RX | regs::MIB_RESET_TX | regs::MIB_RESET_RUNT,
        )?;
        self.regs.write(regs::UMAC_MIB_CTRL, 0)?;

        self.regs.write(regs::UMAC_MAX_FRAME_LEN, MAX_FRAME_LEN)?;

        let rbuf = self.regs.read(regs::RBUF_CTRL)?;
        self.regs
            .write(regs::RBUF_CTRL, rbuf | regs::RBUF_ALIGN_2B)?;
        // The receive checksum engine reports per descriptor and leaves the
        // frame alone, so the 64-byte receive status block stays off and the
        // buffer layout is unchanged. `RBUF_SKIP_FCS` is only for a MAC that
        // forwards the frame check sequence, which this one does not.
        self.regs.write(regs::RBUF_CHK_CTRL, regs::RBUF_RXCHK_EN)?;
        self.regs
            .write(regs::RBUF_TBUF_SIZE_CTRL, regs::TBUF_SIZE_ONE_PORT)?;
        // Every transmitted buffer now begins with the status block that
        // carries the checksum directive; the controller strips it.
        let tbuf = self.regs.read(regs::TBUF_CTRL)?;
        self.regs.write(regs::TBUF_CTRL, tbuf | regs::TBUF_64B_EN)?;

        // The Pi 4 wires an external gigabit PHY on RGMII.
        self.regs
            .write(regs::SYS_PORT_CTRL, regs::PORT_MODE_EXT_GPHY)
    }

    /// Program the link-layer address into the UniMAC's address registers.
    fn write_hwaddr(&mut self) -> Result<(), DriverError> {
        let a = self.mac.as_octets();
        let high = u32::from_be_bytes([a[0], a[1], a[2], a[3]]);
        let low = u32::from(u16::from_be_bytes([a[4], a[5]]));
        self.regs.write(regs::UMAC_MAC0, high)?;
        self.regs.write(regs::UMAC_MAC1, low)
    }

    /// Program the receiver's destination-address filter: this station's own
    /// unicast address and the broadcast address in the first two slots, then
    /// `groups` in the slots after them.
    ///
    /// The address registers are not the receive filter — they identify the
    /// station for MAC control frames — so a controller whose filter slots
    /// are all disabled delivers nothing. The two fixed addresses are the
    /// minimum a host needs to be addressable: without broadcast there is no
    /// ARP and no DHCP offer, and without its own address no unicast reply
    /// arrives. Promiscuous reception is deliberately not used — it would
    /// hand the network stack every frame on the segment, including those
    /// addressed to other hosts.
    fn write_rx_filter(&mut self, groups: &[MacAddress]) -> Result<(), DriverError> {
        // Typed by the constant, so changing it without changing this pair
        // is a compile error rather than a silently mis-sized filter.
        let fixed: [MacAddress; RX_FILTER_ADDRESSES as usize] = [MacAddress::BROADCAST, self.mac];
        // Slot 0 is enabled by the highest bit, each later slot by the next
        // one down.
        let mut enable = 1u32 << (regs::MDF_SLOTS - 1);
        let mut enabled = 0u32;
        for (slot, address) in fixed.iter().chain(groups).enumerate() {
            let octets = address.as_octets();
            let base = regs::UMAC_MDF_ADDR + slot * regs::MDF_SLOT_STRIDE;
            self.regs
                .write(base, u32::from(u16::from_be_bytes([octets[0], octets[1]])))?;
            self.regs.write(
                base + 4,
                u32::from_be_bytes([octets[2], octets[3], octets[4], octets[5]]),
            )?;
            enabled |= enable;
            enable >>= 1;
        }
        self.regs.write(regs::UMAC_MDF_CTRL, enabled)
    }

    /// Stop both DMA engines and drain the transmit path, so the rings can
    /// be reprogrammed with the device quiescent.
    fn disable_dma(&mut self) -> Result<(), DriverError> {
        for desc_base in [regs::RDMA_DESC, regs::TDMA_DESC] {
            let ctrl = regs::dma_regs(desc_base) + regs::DMA_CTRL;
            let current = self.regs.read(ctrl)?;
            self.regs.write(ctrl, current & !regs::DMA_EN)?;
        }
        self.regs.write(regs::UMAC_TX_FLUSH, 1)?;
        self.delay.delay_us(RESET_HOLD_US);
        self.regs.write(regs::UMAC_TX_FLUSH, 0)
    }

    /// Enable both DMA engines and the default ring's buffers.
    fn enable_dma(&mut self) -> Result<(), DriverError> {
        let ctrl_value = regs::DMA_EN | (1 << (regs::DEFAULT_RING + regs::DMA_RING_BUF_EN_SHIFT));
        for desc_base in [regs::RDMA_DESC, regs::TDMA_DESC] {
            let ctrl = regs::dma_regs(desc_base) + regs::DMA_CTRL;
            let current = self.regs.read(ctrl)?;
            self.regs.write(ctrl, current | ctrl_value)?;
        }
        Ok(())
    }

    /// Program the shared ring geometry both directions use, leaving only
    /// the direction-specific registers to the callers.
    fn init_ring_common(&mut self, desc_base: usize) -> Result<(), DriverError> {
        let block = regs::dma_regs(desc_base);
        let ring = regs::ring_regs(desc_base, regs::DEFAULT_RING);
        self.regs
            .write(block + regs::DMA_SCB_BURST_SIZE, regs::DMA_MAX_BURST_LENGTH)?;
        // The ring occupies the first programmed descriptors of the
        // block's RAM; the addresses are descriptor-*word* indices.
        self.regs.write(ring + regs::RING_START_ADDR, 0)?;
        self.regs.write(ring + regs::RING_START_ADDR_HI, 0)?;
        self.regs.write(
            ring + regs::RING_END_ADDR,
            self.layout.ring_slots() * regs::DESC_WORDS - 1,
        )?;
        self.regs.write(ring + regs::RING_END_ADDR_HI, 0)?;
        self.regs.write(ring + regs::RING_RW_POINTER, 0)?;
        self.regs.write(ring + regs::RING_WR_POINTER, 0)?;
        self.regs.write(ring + regs::RING_DEVICE_INDEX, 0)?;
        self.regs.write(ring + regs::RING_DRIVER_INDEX, 0)?;
        self.regs.write(
            ring + regs::RING_BUF_SIZE,
            (self.layout.ring_slots() << regs::RING_SIZE_SHIFT) | BUF_LEN,
        )?;
        self.regs
            .write(block + regs::DMA_RING_CFG, 1 << regs::DEFAULT_RING)
    }

    /// The link as last resolved from the PHY, in the vocabulary both the
    /// device facts and every service report state it in.
    fn link_state(&self) -> LinkState {
        if self.link.is_some() {
            LinkState::Up
        } else {
            LinkState::Down
        }
    }

    /// Program when the default ring raises its completion interrupt: on the
    /// first completed descriptor, with the ring's timer disarmed.
    ///
    /// Both halves are programmed rather than inherited. The condition that
    /// raises `IRQ_*DMA_DONE` is a comparison against this threshold, so a
    /// ring left holding whatever value reset (or the firmware) put there is
    /// a ring whose interrupt behaviour is unknown — and a threshold of zero
    /// is satisfied permanently, which is a level condition draining cannot
    /// clear and so an interrupt storm no masking can end.
    ///
    /// A threshold of one with no timer is the deliberate choice, not a
    /// missing feature: this driver's coalescing comes from masking the
    /// completion sources for the duration of a drain, which adapts to the
    /// actual burst, where a timer would only add latency to a lone frame.
    fn arm_completion_interrupt(&mut self, desc_base: usize) -> Result<(), DriverError> {
        let ring = regs::ring_regs(desc_base, regs::DEFAULT_RING);
        self.regs.write(ring + regs::RING_MBUF_DONE_THRESH, 1)?;
        let timeout = regs::ring_timeout(desc_base, regs::DEFAULT_RING);
        let current = self.regs.read(timeout)?;
        self.regs.write(timeout, current & !regs::DMA_TIMEOUT_MASK)
    }

    /// Build the receive ring: geometry, flow-control thresholds, and one
    /// device-owned descriptor per buffer.
    ///
    /// Receive descriptors are armed once here and never rewritten: the
    /// consumer index alone hands slots back to the device, so the hot path
    /// touches one register per frame rather than a descriptor.
    fn init_rx(&mut self) -> Result<(), DriverError> {
        self.init_ring_common(regs::RDMA_DESC)?;
        let ring = regs::ring_regs(regs::RDMA_DESC, regs::DEFAULT_RING);
        self.regs
            .write(ring + regs::RING_FLOW_PERIOD, regs::DMA_FC_THRESH)?;
        self.arm_completion_interrupt(regs::RDMA_DESC)?;
        let length_status = (BUF_LEN << regs::DMA_BUFLENGTH_SHIFT) | regs::DMA_OWN;
        for slot in 0..self.layout.ring_slots() {
            let (low, high) = address_words(self.rx_buffer_device_addr(slot));
            let desc = regs::desc(regs::RDMA_DESC, slot);
            self.regs.write(desc + regs::DESC_ADDRESS_LO, low)?;
            self.regs.write(desc + regs::DESC_ADDRESS_HI, high)?;
            self.regs
                .write(desc + regs::DESC_LENGTH_STATUS, length_status)?;
        }
        Ok(())
    }

    /// Build the transmit ring: geometry, a one-buffer completion
    /// threshold, and each descriptor's fixed buffer address.
    ///
    /// The buffer a transmit slot uses never changes, so the address words
    /// are written once here and the per-frame path writes only
    /// `length_status`.
    fn init_tx(&mut self) -> Result<(), DriverError> {
        self.init_ring_common(regs::TDMA_DESC)?;
        let ring = regs::ring_regs(regs::TDMA_DESC, regs::DEFAULT_RING);
        self.arm_completion_interrupt(regs::TDMA_DESC)?;
        self.regs.write(ring + regs::RING_FLOW_PERIOD, 0)?;
        for slot in 0..self.layout.ring_slots() {
            let (low, high) = address_words(self.tx_buffer_device_addr(slot));
            let desc = regs::desc(regs::TDMA_DESC, slot);
            self.regs.write(desc + regs::DESC_ADDRESS_LO, low)?;
            self.regs.write(desc + regs::DESC_ADDRESS_HI, high)?;
            self.regs.write(desc + regs::DESC_LENGTH_STATUS, 0)?;
        }
        Ok(())
    }

    /// Program the MAC and the RGMII out-of-band control from the resolved
    /// link, or leave the transmitter and receiver disabled while there is
    /// no link.
    fn apply_link(&mut self) -> Result<(), DriverError> {
        let Some(link) = self.link else {
            let cmd = self.regs.read(regs::UMAC_CMD)?;
            return self
                .regs
                .write(regs::UMAC_CMD, cmd & !(regs::CMD_TX_EN | regs::CMD_RX_EN));
        };
        // Drive the link indication from the MAC and disable the internal
        // transmit-clock delay: the board wires `rgmii-rxid`, so the receive
        // delay is the PHY's and the MAC must add none of its own.
        let oob = self.regs.read(regs::EXT_RGMII_OOB_CTRL)?;
        self.regs.write(
            regs::EXT_RGMII_OOB_CTRL,
            (oob & !regs::OOB_DISABLE) | regs::RGMII_LINK | regs::RGMII_MODE_EN | regs::ID_MODE_DIS,
        )?;
        let speed = link.speed.umac_selector() << regs::CMD_SPEED_SHIFT;
        let cmd =
            self.regs.read(regs::UMAC_CMD)? & !(regs::CMD_SPEED_MASK << regs::CMD_SPEED_SHIFT);
        self.regs.write(
            regs::UMAC_CMD,
            cmd | speed | regs::CMD_TX_EN | regs::CMD_RX_EN,
        )
    }

    /// Re-resolve the link after a PHY link event and re-apply it to the
    /// MAC, so a cable change is followed without a driver restart.
    fn refresh_link(&mut self) -> Result<(), DriverError> {
        self.link_event = false;
        self.link = mdio::resolve(&mut self.regs, &self.delay, PHY_ADDRESS)?;
        self.apply_link()
    }

    /// Device-visible address of receive buffer `slot`.
    fn rx_buffer_device_addr(&self, slot: u32) -> u64 {
        self.frames.phys() + u64::from(slot) * u64::from(BUF_LEN)
    }

    /// Device-visible address of transmit buffer `slot`, which follows the
    /// whole receive-buffer block.
    fn tx_buffer_device_addr(&self, slot: u32) -> u64 {
        self.frames.phys() + u64::from(self.layout.ring_slots() + slot) * u64::from(BUF_LEN)
    }

    /// Byte range of receive buffer `slot` within the carve.
    fn rx_buffer_range(slot: u32) -> (usize, usize) {
        let start = (slot * BUF_LEN) as usize;
        (start, start + BUF_LEN as usize)
    }

    /// Byte range of transmit buffer `slot` within the carve: the transmit
    /// status block, then the frame. Transmit buffers follow the whole
    /// receive-buffer block, so the range depends on the ring depth.
    fn tx_buffer_range(layout: DmaLayout, slot: u32) -> (usize, usize) {
        let start = ((layout.ring_slots() + slot) * BUF_LEN) as usize;
        (start, start + BUF_LEN as usize)
    }

    /// Byte range of the frame area inside transmit buffer `slot`, past the
    /// status block the controller reads first.
    fn tx_frame_range(layout: DmaLayout, slot: u32) -> (usize, usize) {
        let (start, end) = Self::tx_buffer_range(layout, slot);
        (start + regs::TSB_LEN as usize, end)
    }

    /// Transmit descriptors free for a frame right now.
    fn tx_free_slots(slots: u32, producer: u32, consumer: u32) -> u32 {
        slots - Self::in_flight(producer, consumer).min(slots)
    }

    /// Descriptors the device still owns, from a producer/consumer pair.
    fn in_flight(producer: u32, consumer: u32) -> u32 {
        producer.wrapping_sub(consumer) & regs::RING_INDEX_MASK
    }

    /// Read the device's own index register for a ring, masked to the
    /// 16-bit counter (the rest of the register carries a done/discard
    /// count this driver does not use).
    fn device_index(&mut self, desc_base: usize) -> Result<u32, DriverError> {
        let ring = regs::ring_regs(desc_base, regs::DEFAULT_RING);
        Ok(self.regs.read(ring + regs::RING_DEVICE_INDEX)? & regs::RING_INDEX_MASK)
    }

    /// Advance the driver's record of completed transmits, scrubbing each
    /// freed buffer when the ring is carrying sensitive traffic.
    ///
    /// A device that claims to have consumed more descriptors than the driver
    /// ever queued is refused: acting on it would free slots still in flight
    /// and drive the producer index past the ring. A buggy controller is
    /// inside the fault boundary the driver defends, so the report is
    /// validated rather than trusted.
    fn reclaim_tx(&mut self, sensitive: bool) -> Result<(), DriverError> {
        let consumer = self.device_index(regs::TDMA_DESC)?;
        let completed = consumer.wrapping_sub(self.tx_consumer) & regs::RING_INDEX_MASK;
        if completed > self.tx_in_flight() {
            return Err(DriverError::DeviceFault);
        }
        for _ in 0..completed {
            if sensitive {
                let (start, end) =
                    Self::tx_buffer_range(self.layout, self.layout.ring_index(self.tx_consumer));
                self.frames.as_bytes_mut()[start..end].fill(0);
            }
            self.tx_consumer = self.tx_consumer.wrapping_add(1) & regs::RING_INDEX_MASK;
        }
        Ok(())
    }

    /// Transmit descriptors the device still owns.
    fn tx_in_flight(&self) -> u32 {
        Self::in_flight(self.tx_producer, self.tx_consumer)
    }

    /// Move every frame the stack queued into a free transmit slot and ring
    /// the device's producer index, stopping when the ring runs dry or the
    /// device has no free slot left.
    ///
    /// An ordinary frame is popped straight into its slot's buffer. A
    /// segmentation super-frame does not fit one — the transport sizes a ring
    /// slot for it, the device's buffers only for a wire frame — so the
    /// refused pop is what identifies it, and it is staged and split.
    fn drain_tx(
        &mut self,
        rings: &mut FrameRings<'_>,
        report: &mut ServiceReport,
        sensitive: bool,
    ) -> Result<(), DriverError> {
        // A super-frame a full ring left part-split goes first: the wire
        // order of one TCP stream must not be disturbed by a later frame.
        if !self.drain_staged(report, sensitive)? {
            return Ok(());
        }
        loop {
            if self.tx_in_flight() >= self.layout.ring_slots() {
                return Ok(());
            }
            let slot = self.layout.ring_index(self.tx_producer);
            let (start, end) = Self::tx_frame_range(self.layout, slot);
            let mut offload = FrameOffload::None;
            let popped = rings
                .tx
                .pop_with(&mut offload, &mut self.frames.as_bytes_mut()[start..end]);
            let length = match popped {
                Ok(Some(length)) => length,
                Ok(None) => return Ok(()),
                // A corrupt ring slot was consumed by the failed pop; skip it
                // rather than let a malformed producer wedge the frames queued
                // behind it.
                Err(Errno::LengthOutOfRange) => continue,
                // Longer than one wire frame. Only a segmentation super-frame
                // legitimately is; a refused pop leaves the slot, so staging
                // re-reads it and anything else is released and dropped.
                Err(Errno::BufferTooSmall) => {
                    self.stage_super_frame(rings)?;
                    if !self.drain_staged(report, sensitive)? {
                        return Ok(());
                    }
                    continue;
                }
                Err(_) => return Err(DriverError::BadMagic),
            };
            if matches!(offload, FrameOffload::TxSegment { .. }) {
                // A super-frame small enough to have fitted one slot still
                // needs its per-segment header fix-ups, so it takes the same
                // staged path rather than a second, near-identical one.
                self.stage_from_slot(slot, length, offload);
                if !self.drain_staged(report, sensitive)? {
                    return Ok(());
                }
                continue;
            }
            // A runt the device would refuse, or a frame past what the MAC
            // accepts, is consumed and dropped.
            let Ok(length) = u32::try_from(length) else {
                continue;
            };
            if !(ETHERNET_HEADER_LEN..=MAX_FRAME_LEN).contains(&length) {
                continue;
            }
            let layout = self.layout;
            if Self::queue_slot(
                self.frames.as_bytes_mut(),
                &mut self.regs,
                &mut self.tx_producer,
                layout,
                slot,
                length,
                offload,
            )? {
                report.transmitted += 1;
            }
        }
    }

    /// Dequeue the over-size frame at the head of the transmit ring into the
    /// staging area, ready to be split.
    ///
    /// A frame that is over-size for any reason other than segmentation — no
    /// segmentation descriptor, or one the frame does not bear out — is
    /// released and dropped: the device could not have transmitted it, and
    /// leaving it would wedge everything queued behind it.
    fn stage_super_frame(&mut self, rings: &mut FrameRings<'_>) -> Result<(), DriverError> {
        let mut offload = FrameOffload::None;
        let staging = self.layout.buffer_bytes();
        let popped = rings
            .tx
            .pop_with(&mut offload, &mut self.frames.as_bytes_mut()[staging..]);
        // A pop that refuses leaves the slot occupied — longer than a whole
        // ring slot, or corrupt — so release it explicitly rather than let it
        // wedge the queue behind it.
        let Ok(Some(length)) = popped else {
            rings.tx.skip().map_err(|_| DriverError::BadMagic)?;
            return Ok(());
        };
        self.stage(length, offload);
        Ok(())
    }

    /// Move a super-frame that already fits one transmit slot into the
    /// staging area, so both super-frame paths split from one place.
    fn stage_from_slot(&mut self, slot: u32, length: usize, offload: FrameOffload) {
        let (start, _) = Self::tx_frame_range(self.layout, slot);
        let staging = self.layout.buffer_bytes();
        self.frames
            .as_bytes_mut()
            .copy_within(start..start + length, staging);
        self.stage(length, offload);
    }

    /// Accept the staged bytes as a super-frame, unless they do not describe
    /// one — a descriptor the frame does not bear out, or one whose segments
    /// would not fit a transmit buffer, is dropped rather than transmitted
    /// half-fixed-up or split against a buffer that is not there.
    fn stage(&mut self, length: usize, offload: FrameOffload) {
        let staging = self.layout.buffer_bytes();
        let frame = &self.frames.as_bytes()[staging..staging + length];
        let segment_fits = |segmenter: &TcpSegmenter<'_>| {
            u32::try_from(segmenter.max_segment_len()).is_ok_and(|max| max <= MAX_FRAME_LEN)
        };
        self.staged = TcpSegmenter::new(frame, offload)
            .ok()
            .filter(segment_fits)
            .map(|_| Staged {
                length,
                offload,
                emitted: 0,
            });
    }

    /// Zero a finished super-frame's staged bytes when the ring is carrying
    /// sensitive traffic, so its plaintext does not outlive its transmission.
    fn scrub_staging(staging: &mut [u8], length: usize, sensitive: bool) {
        if sensitive {
            if let Some(bytes) = staging.get_mut(..length) {
                bytes.fill(0);
            }
        }
    }

    /// Split the staged super-frame into wire frames, one transmit slot each.
    ///
    /// Returns `false` when the ring filled before the split finished: the
    /// remainder stays staged and the next doorbell — which a transmit
    /// completion interrupt provokes — picks it up, so nothing is dropped and
    /// nothing spins.
    fn drain_staged(
        &mut self,
        report: &mut ServiceReport,
        sensitive: bool,
    ) -> Result<bool, DriverError> {
        let Some(mut staged) = self.staged.take() else {
            return Ok(true);
        };
        let Self {
            regs,
            frames,
            layout,
            tx_producer,
            tx_consumer,
            ..
        } = self;
        let layout = *layout;
        // The staging area sits past every frame buffer, so the source frame
        // and the destination slot are disjoint borrows of the one carve.
        let (buffers, staging) = frames.as_bytes_mut().split_at_mut(layout.buffer_bytes());
        let frame = &staging[..staged.length];
        let Ok(mut segmenter) = TcpSegmenter::resume(frame, staged.offload, staged.emitted) else {
            // Unreachable: `stage` validated the same frame. Dropping it is
            // the fail-closed answer if it ever were reached.
            return Ok(true);
        };
        let offload = segmenter.checksum_offload();
        loop {
            if Self::tx_free_slots(layout.ring_slots(), *tx_producer, *tx_consumer) == 0 {
                staged.emitted = segmenter.emitted();
                self.staged = Some(staged);
                return Ok(false);
            }
            let slot = layout.ring_index(*tx_producer);
            let (start, end) = Self::tx_frame_range(layout, slot);
            // `stage` refused any super-frame whose segments could not fit a
            // buffer, so a short-buffer refusal here cannot arise; treating it
            // as the end of the split drops the remainder rather than wedging
            // the device.
            let Ok(Some(length)) = segmenter.next_segment(&mut buffers[start..end]) else {
                Self::scrub_staging(staging, staged.length, sensitive);
                return Ok(true);
            };
            let Ok(length) = u32::try_from(length) else {
                Self::scrub_staging(staging, staged.length, sensitive);
                return Ok(true);
            };
            if Self::queue_slot(buffers, regs, tx_producer, layout, slot, length, offload)? {
                report.transmitted += 1;
            }
        }
    }

    /// Fill the transmit status block for the frame already sitting in
    /// `slot`'s buffer, program the descriptor, and publish it.
    ///
    /// Returns `false` when the frame was dropped instead: a checksum
    /// directive the frame does not bear out cannot be honoured, and a frame
    /// carrying only a partial checksum must not reach the wire.
    fn queue_slot(
        buffers: &mut [u8],
        regs: &mut R,
        tx_producer: &mut u32,
        layout: DmaLayout,
        slot: u32,
        frame_len: u32,
        offload: FrameOffload,
    ) -> Result<bool, DriverError> {
        let (start, _) = Self::tx_buffer_range(layout, slot);
        let frame_start = start + regs::TSB_LEN as usize;
        let frame_end = frame_start + frame_len as usize;
        let Some(directive) = csum_directive(&buffers[frame_start..frame_end], offload) else {
            return Ok(false);
        };
        // The controller reads only the directive word; the rest is zeroed so
        // no byte of a previous frame is handed to a DMA master.
        let block = &mut buffers[start..frame_start];
        block.fill(0);
        block[regs::TSB_CSUM_INFO..regs::TSB_CSUM_INFO + 4]
            .copy_from_slice(&directive.to_le_bytes());

        let checksummed = if directive == 0 {
            0
        } else {
            regs::DMA_TX_DO_CSUM
        };
        let length_status = ((regs::TSB_LEN + frame_len) << regs::DMA_BUFLENGTH_SHIFT)
            | (regs::DMA_TX_QTAG_MASK << regs::DMA_TX_QTAG_SHIFT)
            | regs::DMA_TX_APPEND_CRC
            | checksummed
            | regs::DMA_SOP
            | regs::DMA_EOP;
        let desc = regs::desc(regs::TDMA_DESC, slot);
        regs.write(desc + regs::DESC_LENGTH_STATUS, length_status)?;
        *tx_producer = tx_producer.wrapping_add(1) & regs::RING_INDEX_MASK;
        let ring = regs::ring_regs(regs::TDMA_DESC, regs::DEFAULT_RING);
        regs.write(ring + regs::RING_DRIVER_INDEX, *tx_producer)?;
        Ok(true)
    }

    /// Deliver every frame the device has completed into the receive ring,
    /// dropping the ones it marked bad and stopping — without loss — when
    /// the ring is full.
    ///
    /// The device's producer report is validated against the ring's capacity
    /// before it is walked, so a corrupt index cannot drive the driver
    /// through descriptors the ring never produced.
    fn harvest_rx(
        &mut self,
        rings: &mut FrameRings<'_>,
        report: &mut ServiceReport,
        sensitive: bool,
    ) -> Result<(), DriverError> {
        let ring = regs::ring_regs(regs::RDMA_DESC, regs::DEFAULT_RING);
        let producer = self.device_index(regs::RDMA_DESC)?;
        // A device that claims more completed descriptors than the ring holds
        // is refused rather than walked: the ring cannot have produced them,
        // so the report is corrupt and acting on it would deliver whatever
        // the descriptor RAM happens to hold.
        let pending = producer.wrapping_sub(self.rx_consumer) & regs::RING_INDEX_MASK;
        if pending > self.layout.ring_slots() {
            return Err(DriverError::DeviceFault);
        }
        for _ in 0..pending {
            let slot = self.layout.ring_index(self.rx_consumer);
            let desc = regs::desc(regs::RDMA_DESC, slot);
            let status = self.regs.read(desc + regs::DESC_LENGTH_STATUS)?;
            match self.deliver_rx(rings, status, slot)? {
                RxOutcome::Delivered => report.record_delivered(),
                RxOutcome::Filtered => {
                    self.filtered_frames += 1;
                    report.record_undelivered();
                }
                RxOutcome::Dropped => report.record_undelivered(),
                // Leave the frame in its descriptor and the slot unfreed:
                // the device cannot overwrite it until the consumer index
                // advances, so the stack drains the ring and calls again.
                RxOutcome::RingFull => {
                    report.rx_ring_full = true;
                    return Ok(());
                }
            }
            if sensitive {
                let (start, end) = Self::rx_buffer_range(slot);
                self.frames.as_bytes_mut()[start..end].fill(0);
            }
            self.rx_consumer = self.rx_consumer.wrapping_add(1) & regs::RING_INDEX_MASK;
            self.regs
                .write(ring + regs::RING_DRIVER_INDEX, self.rx_consumer)?;
        }
        Ok(())
    }

    /// Copy one completed receive descriptor's frame into the ring, or
    /// decide it must be dropped.
    fn deliver_rx(
        &mut self,
        rings: &mut FrameRings<'_>,
        status: u32,
        slot: u32,
    ) -> Result<RxOutcome, DriverError> {
        // A frame the device flagged, or one it split across descriptors
        // (this ring's buffers hold a whole frame, so a fragment is a
        // malformed delivery), is dropped whole.
        if status & regs::DMA_RX_ERRORS != 0
            || status & (regs::DMA_SOP | regs::DMA_EOP) != (regs::DMA_SOP | regs::DMA_EOP)
        {
            return Ok(RxOutcome::Dropped);
        }
        let reported = (status >> regs::DMA_BUFLENGTH_SHIFT) & regs::DMA_BUFLENGTH_MASK;
        // The reported length includes the two-byte alignment pad the
        // receive buffer inserts ahead of the frame.
        if reported <= regs::RX_FRAME_OFFSET || reported > BUF_LEN {
            return Ok(RxOutcome::Dropped);
        }
        let length = (reported - regs::RX_FRAME_OFFSET) as usize;
        if length < ETHERNET_HEADER_LEN as usize {
            return Ok(RxOutcome::Dropped);
        }
        let (start, _) = Self::rx_buffer_range(slot);
        let from = start + regs::RX_FRAME_OFFSET as usize;
        // Bit 15 of a *completed* receive descriptor is the checksum
        // engine's verdict, not the ownership flag it means while the device
        // still holds the descriptor. Absent, the stack simply folds the
        // frame itself.
        let offload = if status & regs::DMA_RX_CHK_OK != 0 {
            FrameOffload::Validated
        } else {
            FrameOffload::None
        };
        // `deliver` applies the shared receive pre-filter, so a frame with
        // no possible local consumer is neither copied nor woken for.
        let frame = &self.frames.as_bytes()[from..from + length];
        match rings.deliver(0, offload, frame) {
            Ok(RxDelivery::Accepted) => Ok(RxOutcome::Delivered),
            Ok(RxDelivery::Filtered) => Ok(RxOutcome::Filtered),
            Ok(RxDelivery::RingFull) => Ok(RxOutcome::RingFull),
            Err(_) => Err(DriverError::BadMagic),
        }
    }
}

/// What became of one completed receive descriptor.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum RxOutcome {
    /// The frame reached the receive ring.
    Delivered,
    /// The device flagged the frame, or it was malformed; the slot is freed
    /// and nothing is delivered.
    Dropped,
    /// The receive pre-filter found no possible local consumer; the slot is
    /// freed and the stack is never woken for it.
    Filtered,
    /// The receive ring is full; the frame stays in its descriptor.
    RingFull,
}

impl<R: GenetRegs, D: Delay> Net for Genet<R, D> {
    fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
        Ok(DeviceFacts {
            mac: self.mac,
            mtu: MTU,
            // The link the driver last resolved. It is not read live here
            // because the register seam needs exclusive access and this
            // report does not: the PHY's link-change interrupt wakes the
            // driver, which re-resolves on its next service doorbell, so the
            // reported state tracks the wire.
            link: self.link_state(),
            offloads: OFFLOADS,
            rx_queues: 1,
            max_tx_frame: self.layout.tx_staging(),
            multicast_filter: McastFilter::Slots(MCAST_SLOTS),
        })
    }

    fn set_multicast_groups(&mut self, groups: &[MacAddress]) -> Result<(), DriverError> {
        if groups.len() > usize::from(MCAST_SLOTS) {
            return Err(DriverError::LengthOutOfRange);
        }
        self.write_rx_filter(groups)
    }

    fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
        let sensitive = rings.class.is_sensitive();
        if self.link_event {
            self.refresh_link()?;
        }
        let mut report = ServiceReport::default();
        self.reclaim_tx(sensitive)?;
        self.drain_tx(rings, &mut report, sensitive)?;
        self.harvest_rx(rings, &mut report, sensitive)?;
        report.filtered = self.filtered_frames;
        // The link as of this doorbell — re-resolved above when the PHY
        // signalled a change. Without it here a cable pull would reach the
        // stack only through a `DeviceFacts` query nothing on the frame path
        // makes, so the interface would read up for ever and a bond would
        // never fail over.
        report.link = self.link_state();
        Ok(report)
    }

    fn ack_interrupt(&mut self) {
        // Clear exactly what is asserted, and record a link event for the
        // next service to re-resolve over MDIO. Clearing alone does not
        // stop a re-fire — the DMA-done bits latch a level condition that
        // is still true while frames are undrained, so it is
        // `set_completion_interrupts` that holds the line off. A register
        // fault here has nowhere to be reported, so it is dropped: the
        // serve loop re-parks and the next doorbell surfaces the fault
        // typed.
        let Ok(status) = self.regs.read(regs::INTRL2_CPU_STAT) else {
            return;
        };
        if status == 0 {
            return;
        }
        let _ = self.regs.write(regs::INTRL2_CPU_CLEAR, status);
        if status & (regs::IRQ_LINK_UP | regs::IRQ_LINK_DOWN) != 0 {
            self.link_event = true;
        }
    }

    fn set_completion_interrupts(&mut self, enabled: bool) -> Result<(), DriverError> {
        // `INTRL2` has separate set/clear mask registers, so neither
        // direction is a read-modify-write and neither can disturb the link
        // sources, which stay unmasked throughout.
        let register = if enabled {
            regs::INTRL2_CPU_MASK_CLEAR
        } else {
            regs::INTRL2_CPU_MASK_SET
        };
        self.regs.write(register, regs::IRQ_COMPLETION)
    }
}

/// The transmit status block's checksum directive for `frame`, or [`None`]
/// when the offload cannot be honoured and the frame must be dropped.
///
/// A directive tells the engine to fold the frame from `csum_start` and store
/// the result at `csum_start + csum_offset`, both relative to the frame's
/// first octet — the status block is not counted. Zero directs it at nothing,
/// which is what a frame already carrying a complete software checksum wants.
///
/// The stack's offsets arrive over a shared-memory ring, so they are
/// re-validated against the frame here: a field past its end would have a DMA
/// master checksum bytes that are not there, and a frame whose partial
/// checksum could not be completed must never reach the wire.
fn csum_directive(frame: &[u8], offload: FrameOffload) -> Option<u32> {
    let FrameOffload::TxChecksum {
        csum_start,
        csum_offset,
    } = offload
    else {
        return Some(0);
    };
    let start = u32::from(csum_start);
    let field = start + u32::from(csum_offset);
    let frame_len = u32::try_from(frame.len()).unwrap_or(u32::MAX);
    if field + 2 > frame_len
        || start > regs::TSB_CSUM_OFFSET_MASK
        || field > regs::TSB_CSUM_OFFSET_MASK
    {
        return None;
    }
    let mut directive = regs::TSB_CSUM_LV | (start << regs::TSB_CSUM_START_SHIFT) | field;
    // RFC 768: a computed UDP checksum of zero is transmitted as `0xFFFF`,
    // and the engine applies that rule only when told the transport is UDP.
    if transport_protocol(frame) == Some(PROTOCOL_UDP) {
        directive |= regs::TSB_CSUM_PROTO_UDP;
    }
    Some(directive)
}

/// Split a device address into the descriptor's `address_lo` and
/// `address_hi` words. Byte-exact and infallible — no cast can drop a bit of
/// a buffer address the device is about to master to.
fn address_words(address: u64) -> (u32, u32) {
    let octets = address.to_le_bytes();
    (
        u32::from_le_bytes([octets[0], octets[1], octets[2], octets[3]]),
        u32::from_le_bytes([octets[4], octets[5], octets[6], octets[7]]),
    )
}
