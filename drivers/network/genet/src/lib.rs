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
//! [`DMA_REGION_BYTES`] carve holds [`RING_SLOTS`] receive and
//! [`RING_SLOTS`] transmit buffers of [`BUF_LEN`] bytes, allocated **once**
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
//! The driver advertises [`NetOffloads::empty`]: the GENET has checksum and
//! segmentation engines, but a driver may advertise only what it has
//! *verified* it can do, and this device is not emulable. The software path
//! in the stack is the canonical implementation, so the NIC is complete
//! without them.
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
    DeviceFacts, LinkState, MacAddress, Net, NetOffloads, ETHERNET_HEADER_LEN, MAC_ADDRESS_LEN,
};
use tairix_abi::driver::net_ring::{FrameRings, ServiceReport};
use tairix_abi::driver::timing::Delay;
use tairix_abi::{
    CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, Errno, HwMatchKey,
    RegisterWindow,
};

pub mod mdio;
pub mod regs;
pub mod wiring;

#[cfg(test)]
mod tests;

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

/// Descriptors per direction the driver programs its ring to use.
///
/// The controller's descriptor RAM holds 256 per direction; a ring may be
/// programmed shorter, and 64 is the working set this driver sizes its DMA
/// carve for: at gigabit line rate 64 × 1500-byte frames is ~96 KB in
/// flight each way, comfortably more than the stack's service latency, for a
/// [`DMA_REGION_BYTES`] carve.
pub const RING_SLOTS: u32 = 64;

/// Bytes per frame buffer. The controller's receive buffer size must cover
/// the largest frame plus the two-byte alignment pad it inserts; 2 KiB is
/// the natural power-of-two above that and keeps each buffer on its own
/// cache-line-aligned stride.
pub const BUF_LEN: u32 = 2048;

/// Bytes of device-visible DMA the driver carves once at [`Genet::open`]:
/// one [`BUF_LEN`] buffer per receive and per transmit descriptor.
pub const DMA_REGION_BYTES: usize = 2 * (RING_SLOTS as usize) * (BUF_LEN as usize);

/// Destination addresses bring-up admits through the receive filter: this
/// station's own unicast address and the broadcast address.
const RX_FILTER_ADDRESSES: usize = 2;

const _: () = assert!(RX_FILTER_ADDRESSES <= regs::MDF_SLOTS as usize);

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
}

impl<R: GenetRegs, D: Delay> Genet<R, D> {
    /// Bring the controller online: verify it really is a GENET v5, reset
    /// the MAC, program `mac`, build both DMA rings over `frames`, start the
    /// PHY, and enable transmit and receive.
    ///
    /// `frames` must be at least [`DMA_REGION_BYTES`] long; a shorter carve
    /// is refused rather than driving the device against buffers that are
    /// not there.
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
    pub fn open(regs: R, delay: D, frames: DmaSlab, mac: MacAddress) -> Result<Self, DriverError> {
        if frames.len() < DMA_REGION_BYTES {
            return Err(DriverError::BufferTooSmall);
        }
        let mut device = Self {
            regs,
            delay,
            frames,
            mac,
            link: None,
            link_event: false,
            rx_consumer: 0,
            tx_producer: 0,
            tx_consumer: 0,
        };
        device.check_revision()?;
        // Mask every level-2 source before touching the device, so a
        // condition left asserted by whatever ran before cannot storm the
        // line while bring-up programs the rings.
        device
            .regs
            .write(regs::INTRL2_CPU_MASK_SET, regs::INTRL2_ALL)?;
        device
            .regs
            .write(regs::INTRL2_CPU_CLEAR, regs::INTRL2_ALL)?;
        device.reset_umac()?;
        device.write_hwaddr()?;
        device.write_rx_filter()?;
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
        self.regs
            .write(regs::RBUF_TBUF_SIZE_CTRL, regs::TBUF_SIZE_ONE_PORT)?;

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

    /// Admit exactly this station's own unicast address and the broadcast
    /// address through the receiver's destination-address filter.
    ///
    /// The address registers are not the receive filter — they identify the
    /// station for MAC control frames — so a controller whose filter slots
    /// are all disabled delivers nothing. Two slots are the minimum a host
    /// needs to be addressable: without broadcast there is no ARP and no
    /// DHCP offer, and without its own address no unicast reply arrives.
    /// Promiscuous reception is deliberately not used — it would hand the
    /// network stack every frame on the segment, including those addressed
    /// to other hosts.
    fn write_rx_filter(&mut self) -> Result<(), DriverError> {
        let admitted: [[u8; MAC_ADDRESS_LEN]; RX_FILTER_ADDRESSES] =
            [*MacAddress::BROADCAST.as_octets(), *self.mac.as_octets()];
        // Slot 0 is enabled by the highest bit, each later slot by the next
        // one down.
        let mut enable = 1u32 << (regs::MDF_SLOTS - 1);
        let mut enabled = 0u32;
        for (slot, address) in admitted.iter().enumerate() {
            let base = regs::UMAC_MDF_ADDR + slot * regs::MDF_SLOT_STRIDE;
            self.regs.write(
                base,
                u32::from(u16::from_be_bytes([address[0], address[1]])),
            )?;
            self.regs.write(
                base + 4,
                u32::from_be_bytes([address[2], address[3], address[4], address[5]]),
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
        // The ring occupies the first `RING_SLOTS` descriptors of the
        // block's RAM; the addresses are descriptor-*word* indices.
        self.regs.write(ring + regs::RING_START_ADDR, 0)?;
        self.regs.write(ring + regs::RING_START_ADDR_HI, 0)?;
        self.regs.write(
            ring + regs::RING_END_ADDR,
            RING_SLOTS * regs::DESC_WORDS - 1,
        )?;
        self.regs.write(ring + regs::RING_END_ADDR_HI, 0)?;
        self.regs.write(ring + regs::RING_RW_POINTER, 0)?;
        self.regs.write(ring + regs::RING_WR_POINTER, 0)?;
        self.regs.write(ring + regs::RING_DEVICE_INDEX, 0)?;
        self.regs.write(ring + regs::RING_DRIVER_INDEX, 0)?;
        self.regs.write(
            ring + regs::RING_BUF_SIZE,
            (RING_SLOTS << regs::RING_SIZE_SHIFT) | BUF_LEN,
        )?;
        self.regs
            .write(block + regs::DMA_RING_CFG, 1 << regs::DEFAULT_RING)
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
        let length_status = (BUF_LEN << regs::DMA_BUFLENGTH_SHIFT) | regs::DMA_OWN;
        for slot in 0..RING_SLOTS {
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
        self.regs.write(ring + regs::RING_MBUF_DONE_THRESH, 1)?;
        self.regs.write(ring + regs::RING_FLOW_PERIOD, 0)?;
        for slot in 0..RING_SLOTS {
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
        self.frames.phys() + u64::from(RING_SLOTS + slot) * u64::from(BUF_LEN)
    }

    /// Byte range of receive buffer `slot` within the carve.
    fn rx_buffer_range(slot: u32) -> (usize, usize) {
        let start = (slot * BUF_LEN) as usize;
        (start, start + BUF_LEN as usize)
    }

    /// Byte range of transmit buffer `slot` within the carve.
    fn tx_buffer_range(slot: u32) -> (usize, usize) {
        let start = ((RING_SLOTS + slot) * BUF_LEN) as usize;
        (start, start + BUF_LEN as usize)
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
                let (start, end) = Self::tx_buffer_range(self.tx_consumer % RING_SLOTS);
                self.frames.as_bytes_mut()[start..end].fill(0);
            }
            self.tx_consumer = self.tx_consumer.wrapping_add(1) & regs::RING_INDEX_MASK;
        }
        Ok(())
    }

    /// Transmit descriptors the device still owns.
    fn tx_in_flight(&self) -> u32 {
        self.tx_producer.wrapping_sub(self.tx_consumer) & regs::RING_INDEX_MASK
    }

    /// Move every frame the stack queued into a free transmit slot and ring
    /// the device's producer index, stopping when the ring runs dry or the
    /// device has no free slot left.
    fn drain_tx(
        &mut self,
        rings: &mut FrameRings<'_>,
        report: &mut ServiceReport,
    ) -> Result<(), DriverError> {
        let ring = regs::ring_regs(regs::TDMA_DESC, regs::DEFAULT_RING);
        loop {
            if self.tx_in_flight() >= RING_SLOTS {
                return Ok(());
            }
            let slot = self.tx_producer % RING_SLOTS;
            let (start, end) = Self::tx_buffer_range(slot);
            let length = match rings.tx.pop(&mut self.frames.as_bytes_mut()[start..end]) {
                Ok(Some(length)) => length,
                Ok(None) => return Ok(()),
                // A corrupt ring slot was consumed by the failed pop; skip it
                // rather than let a malformed producer wedge the frames queued
                // behind it.
                Err(Errno::LengthOutOfRange) => continue,
                // The ring's slots are wider than this device's buffers, and
                // the queued frame does not fit one. It is longer than the MAC
                // would accept anyway, so release the slot explicitly (a
                // refused pop leaves it) and keep the queue flowing.
                Err(Errno::BufferTooSmall) => {
                    rings.tx.skip().map_err(|_| DriverError::BadMagic)?;
                    continue;
                }
                Err(_) => return Err(DriverError::BadMagic),
            };
            // A runt the device would refuse, or a frame past what the MAC
            // accepts, is consumed and dropped for the same reason.
            let Ok(length) = u32::try_from(length) else {
                continue;
            };
            if !(ETHERNET_HEADER_LEN..=MAX_FRAME_LEN).contains(&length) {
                continue;
            }
            let length_status = (length << regs::DMA_BUFLENGTH_SHIFT)
                | (regs::DMA_TX_QTAG_MASK << regs::DMA_TX_QTAG_SHIFT)
                | regs::DMA_TX_APPEND_CRC
                | regs::DMA_SOP
                | regs::DMA_EOP;
            let desc = regs::desc(regs::TDMA_DESC, slot);
            self.regs
                .write(desc + regs::DESC_LENGTH_STATUS, length_status)?;
            self.tx_producer = self.tx_producer.wrapping_add(1) & regs::RING_INDEX_MASK;
            self.regs
                .write(ring + regs::RING_DRIVER_INDEX, self.tx_producer)?;
            report.transmitted += 1;
        }
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
        if pending > RING_SLOTS {
            return Err(DriverError::DeviceFault);
        }
        for _ in 0..pending {
            let slot = self.rx_consumer % RING_SLOTS;
            let desc = regs::desc(regs::RDMA_DESC, slot);
            let status = self.regs.read(desc + regs::DESC_LENGTH_STATUS)?;
            match self.deliver_rx(rings, status, slot)? {
                RxOutcome::Delivered => report.received += 1,
                RxOutcome::Dropped => {}
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
        let rx = rings.rx_ring(0).map_err(|_| DriverError::BadMagic)?;
        match rx.push(&self.frames.as_bytes()[from..from + length]) {
            Ok(()) => Ok(RxOutcome::Delivered),
            Err(Errno::NoSpace) => Ok(RxOutcome::RingFull),
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
            link: if self.link.is_some() {
                LinkState::Up
            } else {
                LinkState::Down
            },
            offloads: NetOffloads::empty(),
            rx_queues: 1,
        })
    }

    fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
        let sensitive = rings.class.is_sensitive();
        if self.link_event {
            self.refresh_link()?;
        }
        let mut report = ServiceReport::default();
        self.reclaim_tx(sensitive)?;
        self.drain_tx(rings, &mut report)?;
        self.harvest_rx(rings, &mut report, sensitive)?;
        Ok(report)
    }

    fn ack_interrupt(&mut self) {
        // Clear exactly what is asserted, so the line is deasserted before
        // it is re-enabled and cannot re-fire in a storm. A register fault
        // here has nowhere to be reported, so it is dropped: the serve loop
        // re-parks and the next doorbell surfaces the fault typed.
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
