//! xHCI device enumeration (xHCI 1.2 §4.3) and the HID interrupt-IN
//! report path.
//!
//! [`UsbDevice`] drives one controller through the full bring-up of a
//! single attached HID device: port reset, Enable Slot, Address
//! Device, `GET_DESCRIPTOR(device)`, `SET_PROTOCOL(boot)`, Configure
//! Endpoint, and a primed interrupt-IN transfer ring. It then
//! implements the [`ReportSource`] seam from
//! `rustos_abi::driver::input`, so the `drivers/input/usb_hid`
//! decoders consume reports straight off the transfer ring.
//!
//! # Memory seam
//!
//! Every byte the controller shares with the driver lives in one
//! caller-provided region behind the [`DmaRegion`] trait — on metal a
//! capability-granted [`DmaSlab`], in host tests a plain shared
//! buffer — so the enumeration state machine is proven host-side
//! against the register-level mock plus an in-memory ring model
//! (`AGENTS.md` §2.2). The engine performs every ring read/write
//! through the seam; the ring state machines themselves hold no
//! memory ([`ProducerRing`], [`EventRingCursor`]).

use rustos_abi::driver::dma::DmaSlab;
use rustos_abi::driver::input::ReportSource;
use rustos_abi::{DriverError, HwDeviceClass, HwMatchKey, HwNode};

use crate::ring::{EventRingCursor, ProducerRing, PushOutcome};
use crate::trb::{self, CompletionCode, Trb, TrbType};
use crate::{DmaProgram, Xhci, XhciHost};

/// Device-shared memory the engine and the controller both see.
///
/// `phys` is the device-visible base; reads and writes are CPU-side
/// and bounds-checked. The implementor owns DMA publication ordering
/// (cache cleaning/invalidation on a non-coherent interconnect).
pub trait DmaRegion {
    /// Device-visible base address of the region.
    fn phys(&self) -> u64;

    /// Byte length of the region.
    fn len(&self) -> usize;

    /// `true` iff the region is zero-length.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy `buf.len()` bytes at `offset` into `buf`.
    ///
    /// # Errors
    ///
    /// [`DriverError::OutOfRange`] if `offset + buf.len()` exceeds
    /// the region.
    fn read(&mut self, offset: usize, buf: &mut [u8]) -> Result<(), DriverError>;

    /// Publish `bytes` at `offset`.
    ///
    /// # Errors
    ///
    /// [`DriverError::OutOfRange`] if `offset + bytes.len()` exceeds
    /// the region.
    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<(), DriverError>;
}

/// TRB slots in the command, EP0, and interrupt transfer rings and in
/// the event segment. Protocol working sets for one device, not
/// scalable capacities (`AGENTS.md` §24.4): each ring only ever holds
/// the single in-flight command or control TD plus the primed
/// interrupt TRBs below.
pub const RING_TRBS: usize = 8;

/// Interrupt-IN transfers kept primed on the transfer ring, so the
/// device always has somewhere to deliver the next report.
pub const PRIMED_REPORTS: usize = 4;

/// Byte length of one HID boot-protocol report buffer (USB HID 1.11
/// App. B: keyboard 8, mouse 3..=8).
pub const REPORT_LEN: usize = 8;

/// Byte length of the control-transfer data buffer (holds the 18-byte
/// device descriptor).
const CTRL_DATA_LEN: usize = 64;

/// Contexts in an input context: the input control context, the slot
/// context, and the 31 endpoint contexts (§6.2.5).
const INPUT_CONTEXTS: usize = 33;

/// Contexts in an output device context: slot + 31 endpoints (§6.2.1).
const OUTPUT_CONTEXTS: usize = 32;

/// Dwords of a context this driver writes (the defined fields all sit
/// in the first eight dwords; a 64-byte context's tail stays zero).
const CTX_DWORDS: usize = 8;

/// Endpoint context type field: Control (§6.2.3).
const EP_TYPE_CONTROL: u32 = 4;

/// Endpoint context type field: Interrupt IN (§6.2.3).
const EP_TYPE_INTERRUPT_IN: u32 = 7;

/// Device Context Index of the default control endpoint (§4.5.1).
const DCI_CONTROL: u8 = 1;

/// Device Context Index of endpoint 1 IN — the boot-protocol HID
/// interrupt endpoint this driver services (§4.5.1: `2 * 1 + 1`).
const DCI_INTERRUPT_IN: u8 = 3;

/// Where each structure lives inside the caller's [`DmaRegion`].
///
/// All offsets are 64-byte aligned — the strictest alignment any of
/// the structures requires (§6.1).
#[derive(Copy, Clone, Debug)]
struct Layout {
    dcbaa: usize,
    erst: usize,
    command_ring: usize,
    event_segment: usize,
    input_ctx: usize,
    output_ctx: usize,
    ep0_ring: usize,
    int_ring: usize,
    ctrl_data: usize,
    report_bufs: usize,
    ctx_size: usize,
    total: usize,
}

impl DmaRegion for DmaSlab {
    fn phys(&self) -> u64 {
        DmaSlab::phys(self)
    }

    fn len(&self) -> usize {
        DmaSlab::len(self)
    }

    fn read(&mut self, offset: usize, buf: &mut [u8]) -> Result<(), DriverError> {
        let end = offset
            .checked_add(buf.len())
            .ok_or(DriverError::OutOfRange)?;
        let bytes = self.as_bytes();
        if end > bytes.len() {
            return Err(DriverError::OutOfRange);
        }
        buf.copy_from_slice(&bytes[offset..end]);
        Ok(())
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<(), DriverError> {
        let end = offset
            .checked_add(bytes.len())
            .ok_or(DriverError::OutOfRange)?;
        let dst = self.as_bytes_mut();
        if end > dst.len() {
            return Err(DriverError::OutOfRange);
        }
        dst[offset..end].copy_from_slice(bytes);
        Ok(())
    }
}

impl Layout {
    /// Compute the layout for a controller with `max_slots` device
    /// slots and `csz` context size, inside a region of `region_len`
    /// bytes at device-visible base `phys`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `phys` is zero or not 64-byte
    ///   aligned (§6.1).
    /// * [`DriverError::LengthOutOfRange`] if the region cannot hold
    ///   every structure.
    fn new(max_slots: u8, csz: bool, region_len: usize, phys: u64) -> Result<Self, DriverError> {
        if phys == 0 || phys % 64 != 0 {
            return Err(DriverError::OutOfRange);
        }
        let ctx_size = if csz { 64 } else { 32 };
        let mut next = 0usize;
        let mut take = |len: usize| -> usize {
            let offset = next;
            next = (next + len).next_multiple_of(64);
            offset
        };
        let dcbaa = take((usize::from(max_slots) + 1) * 8);
        let erst = take(16);
        let command_ring = take(RING_TRBS * trb::TRB_LEN);
        let event_segment = take(RING_TRBS * trb::TRB_LEN);
        let input_ctx = take(INPUT_CONTEXTS * ctx_size);
        let output_ctx = take(OUTPUT_CONTEXTS * ctx_size);
        let ep0_ring = take(RING_TRBS * trb::TRB_LEN);
        let int_ring = take(RING_TRBS * trb::TRB_LEN);
        let ctrl_data = take(CTRL_DATA_LEN);
        let report_bufs = take(RING_TRBS * REPORT_LEN);
        let total = next;
        if total > region_len {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok(Self {
            dcbaa,
            erst,
            command_ring,
            event_segment,
            input_ctx,
            output_ctx,
            ep0_ring,
            int_ring,
            ctrl_data,
            report_bufs,
            ctx_size,
            total,
        })
    }

    /// Offset of context `index` inside the input context (§6.2.5:
    /// index 0 is the input control context, 1 the slot context, and
    /// `1 + dci` the endpoint contexts).
    fn input_ctx_entry(&self, index: usize) -> usize {
        self.input_ctx + index * self.ctx_size
    }
}

/// Default-control-endpoint max packet size for a protocol speed ID
/// (§4.3 / USB2 §5.5.3, USB3 §9.6.6).
const fn ep0_max_packet(speed: u8) -> Result<u32, DriverError> {
    match speed {
        // Low speed.
        2 => Ok(8),
        // Full and high speed (full speed's worst case is used until
        // the device descriptor reports otherwise; 64 is universally
        // legal for the fixed-format requests this driver issues).
        1 | 3 => Ok(64),
        // SuperSpeed.
        4 => Ok(512),
        _ => Err(DriverError::DeviceFault),
    }
}

/// The 8-byte SETUP payload of `GET_DESCRIPTOR(device)` for `len`
/// descriptor bytes (USB 2.0 §9.4.3).
const fn setup_get_device_descriptor(len: u16) -> [u8; 8] {
    let l = len.to_le_bytes();
    [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, l[0], l[1]]
}

/// The 8-byte SETUP payload of the HID `SET_PROTOCOL(boot)` class
/// request to `interface` (USB HID 1.11 §7.2.6).
const fn setup_set_protocol_boot(interface: u8) -> [u8; 8] {
    [0x21, 0x0B, 0x00, 0x00, interface, 0x00, 0x00, 0x00]
}

/// The 8-byte SETUP payload of `GET_DESCRIPTOR(configuration, 0)` for
/// `len` bytes (USB 2.0 §9.4.3): descriptor type `0x02` in the high
/// byte of `wValue`, configuration index `0` in the low byte.
const fn setup_get_configuration_descriptor(len: u16) -> [u8; 8] {
    let l = len.to_le_bytes();
    [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, l[0], l[1]]
}

/// `bDescriptorType` of a configuration descriptor (USB 2.0 §9.4
/// Table 9-5).
const DESC_TYPE_CONFIGURATION: u8 = 0x02;

/// `bDescriptorType` of an interface descriptor.
const DESC_TYPE_INTERFACE: u8 = 0x04;

/// The fields of the 18-byte USB device descriptor this driver uses
/// (USB 2.0 §9.6.1), decoded fail-closed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DeviceDescriptor {
    /// `idVendor`.
    pub vendor_id: u16,
    /// `idProduct`.
    pub product_id: u16,
    /// `bDeviceClass` (`0` defers the class to the interfaces — the
    /// usual shape for HID devices).
    pub device_class: u8,
    /// `bNumConfigurations`.
    pub num_configurations: u8,
}

impl DeviceDescriptor {
    /// Byte length of the descriptor on the wire.
    pub const LEN: usize = 18;

    /// Decode the 18 descriptor bytes.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] if `bLength` or `bDescriptorType`
    ///   does not describe a device descriptor, or the device reports
    ///   zero configurations — a forged or corrupt reply (`AGENTS.md`
    ///   §5.4).
    pub fn decode(bytes: &[u8; Self::LEN]) -> Result<Self, DriverError> {
        if usize::from(bytes[0]) < Self::LEN || bytes[1] != 0x01 || bytes[17] == 0 {
            return Err(DriverError::BadMagic);
        }
        Ok(Self {
            vendor_id: u16::from_le_bytes([bytes[8], bytes[9]]),
            product_id: u16::from_le_bytes([bytes[10], bytes[11]]),
            device_class: bytes[4],
            num_configurations: bytes[17],
        })
    }
}

/// The 8-byte SETUP payload of `SET_CONFIGURATION(value)` (USB 2.0
/// §9.4.7) — class requests like `SET_PROTOCOL` are only defined on a
/// configured device.
const fn setup_set_configuration(value: u8) -> [u8; 8] {
    [0x00, 0x09, value, 0x00, 0x00, 0x00, 0x00, 0x00]
}

/// The fields of the configuration descriptor and its first interface
/// descriptor this driver needs (USB 2.0 §9.6.3 / §9.6.5), decoded
/// fail-closed from the concatenated descriptor bytes the device
/// returns for `GET_DESCRIPTOR(configuration)`.
///
/// The interface's class triple is read from the device — never
/// assumed — so the hardware-tree child node the bus emits
/// ([`UsbDevice::describe_device`]) carries the honest class
/// (`AGENTS.md` §18.5).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct InterfaceInfo {
    /// `bConfigurationValue` to select with `SET_CONFIGURATION`.
    pub configuration_value: u8,
    /// `bInterfaceNumber` of the matched interface (the target of the
    /// HID `SET_PROTOCOL` class request).
    pub interface_number: u8,
    /// The 24-bit USB interface class code
    /// `(bInterfaceClass << 16) | (bInterfaceSubClass << 8) | bInterfaceProtocol`
    /// (e.g. an HID boot keyboard is `0x03_01_01`, a boot mouse
    /// `0x03_01_02`), as carried by [`HwMatchKey::usb`].
    pub class24: u32,
}

impl InterfaceInfo {
    /// Byte length of a configuration descriptor header (USB 2.0
    /// §9.6.3) and of an interface descriptor (§9.6.5).
    const CONFIG_HEADER_LEN: usize = 9;
    const INTERFACE_LEN: usize = 9;

    /// Decode the `buf` bytes the device delivered for
    /// `GET_DESCRIPTOR(configuration)` into the configuration value and
    /// its **first** interface descriptor's number and class triple.
    ///
    /// The concatenated descriptors are walked by each descriptor's
    /// `bLength` (USB 2.0 §9.4.3) to the first interface descriptor; a
    /// HID device's class lives on its interface, not the device
    /// descriptor (whose `bDeviceClass` is `0`).
    ///
    /// # Errors
    ///
    /// [`DriverError::BadMagic`] if the leading descriptor is not a
    /// configuration descriptor, a descriptor claims a length that runs
    /// off the buffer or is below the two-byte header, or no interface
    /// descriptor is present — a forged or corrupt reply (`AGENTS.md`
    /// §5.4 / §2.9).
    pub fn decode(buf: &[u8]) -> Result<Self, DriverError> {
        if buf.len() < Self::CONFIG_HEADER_LEN
            || usize::from(buf[0]) < Self::CONFIG_HEADER_LEN
            || buf[1] != DESC_TYPE_CONFIGURATION
        {
            return Err(DriverError::BadMagic);
        }
        let configuration_value = buf[5];
        let mut offset = usize::from(buf[0]);
        while offset + 2 <= buf.len() {
            let length = usize::from(buf[offset]);
            let end = offset.checked_add(length).ok_or(DriverError::BadMagic)?;
            if length < 2 || end > buf.len() {
                return Err(DriverError::BadMagic);
            }
            if buf[offset + 1] == DESC_TYPE_INTERFACE {
                if length < Self::INTERFACE_LEN {
                    return Err(DriverError::BadMagic);
                }
                return Ok(Self {
                    configuration_value,
                    interface_number: buf[offset + 2],
                    class24: (u32::from(buf[offset + 5]) << 16)
                        | (u32::from(buf[offset + 6]) << 8)
                        | u32::from(buf[offset + 7]),
                });
            }
            offset = end;
        }
        Err(DriverError::BadMagic)
    }
}

/// Identity of the enumerated HID device, captured during
/// [`UsbDevice::enumerate_hid`] so the bus can emit it as a discovered
/// hardware-tree child node ([`UsbDevice::describe_device`]).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct HidIdentity {
    vendor_id: u16,
    product_id: u16,
    interface_class: u32,
}

/// Interrupt-IN endpoint service interval, in xHCI `2^(n) * 125 µs`
/// units (§6.2.3.6): `6` is an 8 ms interval, the closest encoding at
/// or below the canonical 10 ms boot-keyboard `bInterval`.
const INT_EP_INTERVAL: u32 = 6;

/// Input control context dwords: dword 1 carries the Add Context
/// flags (`A0` = slot context, `A(dci)` = that endpoint, §6.2.5.1).
fn input_control_dwords(add_flags: u32) -> [u32; CTX_DWORDS] {
    let mut dwords = [0; CTX_DWORDS];
    dwords[1] = add_flags;
    dwords
}

/// Slot context dwords (§6.2.2): protocol speed ID, context entries
/// (the highest DCI in use), and the root-hub port number.
fn slot_ctx_dwords(speed: u8, context_entries: u32, root_port: u8) -> [u32; CTX_DWORDS] {
    let mut dwords = [0; CTX_DWORDS];
    dwords[0] = (u32::from(speed) << 20) | (context_entries << 27);
    dwords[1] = u32::from(root_port) << 16;
    dwords
}

/// Endpoint context dwords (§6.2.3): error count 3, the endpoint
/// type, max packet size, service interval, the transfer ring dequeue
/// pointer with Dequeue Cycle State 1, and the average TRB length.
fn ep_ctx_dwords(ep_type: u32, max_packet: u32, interval: u32, ring: u64) -> [u32; CTX_DWORDS] {
    let mut dwords = [0; CTX_DWORDS];
    dwords[0] = interval << 16;
    dwords[1] = (3 << 1) | (ep_type << 3) | (max_packet << 16);
    let dequeue = ring | 1;
    dwords[2] = crate::low_dword(dequeue);
    dwords[3] = crate::high_dword(dequeue);
    dwords[4] = max_packet;
    dwords
}

/// Publish one [`PushOutcome`] into the ring at `ring_offset`: the
/// data TRB first, then — when the push wrapped — the re-cycled Link
/// TRB (§4.9.2.1 ordering).
fn publish<M: DmaRegion>(
    dma: &mut M,
    ring_offset: usize,
    link_slot: usize,
    outcome: &PushOutcome,
) -> Result<(), DriverError> {
    dma.write(
        ring_offset + outcome.slot * trb::TRB_LEN,
        &outcome.trb.to_bytes(),
    )?;
    if let Some(link) = outcome.link {
        dma.write(ring_offset + link_slot * trb::TRB_LEN, &link.to_bytes())?;
    }
    Ok(())
}

/// One enumerated HID device on a started xHCI controller.
///
/// [`UsbDevice::start`] lays the DMA structures out, programs them
/// through [`Xhci::start`], and leaves the controller running.
/// [`UsbDevice::enumerate_hid`] then brings the device on `port` to
/// the configured, boot-protocol state with a primed interrupt-IN
/// ring, after which [`ReportSource::next_report`] drains reports for
/// the `drivers/input/usb_hid` decoders.
pub struct UsbDevice<H: XhciHost, M: DmaRegion> {
    xhci: Xhci<H>,
    dma: M,
    layout: Layout,
    command_ring: ProducerRing,
    ep0_ring: ProducerRing,
    int_ring: ProducerRing,
    event_cursor: EventRingCursor,
    budget: u32,
    slot: u8,
    identity: Option<HidIdentity>,
}

impl<H: XhciHost, M: DmaRegion> UsbDevice<H, M> {
    /// Lay out and zero the DMA structures inside `dma`, program them,
    /// and start the controller.
    ///
    /// `budget` bounds every wait this engine performs (register
    /// polls and event-ring polls), failing closed on a stuck
    /// controller (`AGENTS.md` §2.1).
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `dma` is not 64-byte aligned.
    /// * [`DriverError::LengthOutOfRange`] if `dma` cannot hold the
    ///   structures (the 64-byte-aligned layout described in the
    ///   module docs).
    /// * [`DriverError::DeviceFault`] if the controller does not
    ///   start within `budget` polls.
    pub fn start(xhci: Xhci<H>, dma: M, budget: u32) -> Result<Self, DriverError> {
        let mut xhci = xhci;
        let mut dma = dma;
        let layout = Layout::new(xhci.max_slots(), xhci.csz(), dma.len(), dma.phys())?;

        let zeros = [0u8; 64];
        let mut offset = 0;
        while offset < layout.total {
            let chunk = (layout.total - offset).min(zeros.len());
            dma.write(offset, &zeros[..chunk])?;
            offset += chunk;
        }

        // The single event ring segment table entry: segment base and
        // size in TRBs (§6.5).
        let event_phys = dma.phys() + layout.event_segment as u64;
        let segment_trbs = u32::try_from(RING_TRBS).map_err(|_| DriverError::LengthOutOfRange)?;
        let mut erst = [0u8; 16];
        erst[..8].copy_from_slice(&event_phys.to_le_bytes());
        erst[8..12].copy_from_slice(&segment_trbs.to_le_bytes());
        dma.write(layout.erst, &erst)?;

        let mut make_ring = |offset: usize| -> Result<ProducerRing, DriverError> {
            let (ring, link) = ProducerRing::new(RING_TRBS, dma.phys() + offset as u64)?;
            dma.write(offset + ring.link_slot() * trb::TRB_LEN, &link.to_bytes())?;
            Ok(ring)
        };
        let command_ring = make_ring(layout.command_ring)?;
        let ep0_ring = make_ring(layout.ep0_ring)?;
        let int_ring = make_ring(layout.int_ring)?;
        let event_cursor = EventRingCursor::new(RING_TRBS)?;

        xhci.start(
            &DmaProgram {
                dcbaap: dma.phys() + layout.dcbaa as u64,
                command_ring: dma.phys() + layout.command_ring as u64,
                erst: dma.phys() + layout.erst as u64,
                event_segment: event_phys,
            },
            budget,
        )?;

        Ok(Self {
            xhci,
            dma,
            layout,
            command_ring,
            ep0_ring,
            int_ring,
            event_cursor,
            budget,
            slot: 0,
            identity: None,
        })
    }

    /// The slot ID the enumerated device occupies (`0` before
    /// [`Self::enumerate_hid`] succeeds).
    #[must_use]
    pub const fn slot(&self) -> u8 {
        self.slot
    }

    /// Device-visible address of byte `offset` within the region.
    fn phys_of(&self, offset: usize) -> u64 {
        self.dma.phys() + offset as u64
    }

    /// Consume the next controller event, advancing `ERDP` when one
    /// was taken.
    fn poll_event(&mut self) -> Result<Option<Trb>, DriverError> {
        let mut bytes = [0u8; RING_TRBS * trb::TRB_LEN];
        self.dma.read(self.layout.event_segment, &mut bytes)?;
        let mut trbs = [Trb::ZERO; RING_TRBS];
        for (index, slot) in trbs.iter_mut().enumerate() {
            let mut image = [0u8; trb::TRB_LEN];
            image.copy_from_slice(&bytes[index * trb::TRB_LEN..(index + 1) * trb::TRB_LEN]);
            *slot = Trb::from_bytes(image);
        }
        let event = self.event_cursor.pop(&trbs)?;
        if event.is_some() {
            let erdp = self.phys_of(self.layout.event_segment)
                + (self.event_cursor.dequeue_index() * trb::TRB_LEN) as u64;
            self.xhci.ack_event(erdp)?;
        }
        Ok(event)
    }

    /// Wait for a completion event for one of `addresses` (the TRBs in
    /// flight), skipping informational port-status-change events.
    ///
    /// A completion for a TRB never issued, an undecodable completion
    /// code, or an unexpected event type is a controller fault,
    /// surfaced rather than absorbed (`AGENTS.md` §2.9).
    fn await_event_for(&mut self, addresses: &[u64]) -> Result<Trb, DriverError> {
        for _ in 0..self.budget {
            let Some(event) = self.poll_event()? else {
                continue;
            };
            match event.trb_type() {
                Ok(TrbType::PortStatusChange) => {}
                Ok(TrbType::CommandCompletion | TrbType::TransferEvent) => {
                    if !addresses.contains(&event.parameter) {
                        return Err(DriverError::DeviceFault);
                    }
                    event.completion_code()?;
                    return Ok(event);
                }
                _ => return Err(DriverError::DeviceFault),
            }
        }
        Err(DriverError::DeviceFault)
    }

    /// Issue one command TRB and wait for its successful completion.
    fn command(&mut self, command: Trb) -> Result<Trb, DriverError> {
        let outcome = self.command_ring.push(command)?;
        publish(
            &mut self.dma,
            self.layout.command_ring,
            self.command_ring.link_slot(),
            &outcome,
        )?;
        self.xhci.ring_doorbell(0, 0)?;
        let event = self.await_event_for(&[outcome.address])?;
        if event.trb_type() != Ok(TrbType::CommandCompletion)
            || event.completion_code() != Ok(CompletionCode::Success)
        {
            return Err(DriverError::DeviceFault);
        }
        self.command_ring.retire_one()?;
        Ok(event)
    }

    /// Write context `index` of the input context (§6.2.5).
    fn write_input_ctx(
        &mut self,
        index: usize,
        dwords: &[u32; CTX_DWORDS],
    ) -> Result<(), DriverError> {
        let mut bytes = [0u8; CTX_DWORDS * 4];
        for (dword_index, dword) in dwords.iter().enumerate() {
            bytes[dword_index * 4..dword_index * 4 + 4].copy_from_slice(&dword.to_le_bytes());
        }
        self.dma.write(self.layout.input_ctx_entry(index), &bytes)
    }

    /// Run one control transfer on the default endpoint: `setup`,
    /// an optional IN data stage of `data_in_len` bytes into the
    /// control data buffer, and the status stage. Returns the bytes
    /// the device actually delivered.
    fn control(&mut self, setup: [u8; 8], data_in_len: u32) -> Result<u32, DriverError> {
        if data_in_len as usize > CTRL_DATA_LEN {
            return Err(DriverError::LengthOutOfRange);
        }
        let transfer_type = if data_in_len > 0 {
            trb::SETUP_TRT_IN
        } else {
            trb::SETUP_TRT_NO_DATA
        };
        let setup_trb = Trb::new(
            TrbType::SetupStage,
            u64::from_le_bytes(setup),
            8,
            trb::CONTROL_IDT | transfer_type,
        );
        let outcome = self.ep0_ring.push(setup_trb)?;
        publish(
            &mut self.dma,
            self.layout.ep0_ring,
            self.ep0_ring.link_slot(),
            &outcome,
        )?;
        let mut data_address = None;
        if data_in_len > 0 {
            let data_trb = Trb::new(
                TrbType::DataStage,
                self.phys_of(self.layout.ctrl_data),
                data_in_len,
                trb::CONTROL_DIR_IN | trb::CONTROL_ISP,
            );
            let outcome = self.ep0_ring.push(data_trb)?;
            publish(
                &mut self.dma,
                self.layout.ep0_ring,
                self.ep0_ring.link_slot(),
                &outcome,
            )?;
            data_address = Some(outcome.address);
        }
        // The status stage runs opposite to the data direction; with
        // no data stage it is always IN (§4.11.2.2).
        let status_direction = if data_in_len > 0 {
            0
        } else {
            trb::CONTROL_DIR_IN
        };
        let status_trb = Trb::new(
            TrbType::StatusStage,
            0,
            0,
            status_direction | trb::CONTROL_IOC,
        );
        let status = self.ep0_ring.push(status_trb)?;
        publish(
            &mut self.dma,
            self.layout.ep0_ring,
            self.ep0_ring.link_slot(),
            &status,
        )?;
        self.xhci.ring_doorbell(self.slot, u32::from(DCI_CONTROL))?;

        // At most two events arrive: a short-packet event for the data
        // stage, then the status-stage completion.
        let mut residual = 0;
        for _ in 0..2 {
            let watch = [data_address.unwrap_or(status.address), status.address];
            let event = self.await_event_for(&watch)?;
            if event.trb_type() != Ok(TrbType::TransferEvent)
                || event.slot_id() != self.slot
                || event.endpoint_id() != DCI_CONTROL
            {
                return Err(DriverError::DeviceFault);
            }
            match event.completion_code() {
                Ok(CompletionCode::Success | CompletionCode::ShortPacket) => {}
                _ => return Err(DriverError::DeviceFault),
            }
            if data_address == Some(event.parameter) {
                residual = event.transfer_residual();
                continue;
            }
            while self.ep0_ring.in_flight() > 0 {
                self.ep0_ring.retire_one()?;
            }
            return data_in_len
                .checked_sub(residual)
                .ok_or(DriverError::DeviceFault);
        }
        Err(DriverError::DeviceFault)
    }

    /// Prime one interrupt-IN transfer: a Normal TRB pointing at the
    /// report buffer paired with the slot it lands in.
    fn arm_report(&mut self) -> Result<(), DriverError> {
        let slot = self.int_ring.enqueue_slot();
        let buffer = self.phys_of(self.layout.report_bufs + slot * REPORT_LEN);
        let report_len = u32::try_from(REPORT_LEN).map_err(|_| DriverError::LengthOutOfRange)?;
        let normal = Trb::new(
            TrbType::Normal,
            buffer,
            report_len,
            trb::CONTROL_IOC | trb::CONTROL_ISP,
        );
        let outcome = self.int_ring.push(normal)?;
        publish(
            &mut self.dma,
            self.layout.int_ring,
            self.int_ring.link_slot(),
            &outcome,
        )
    }

    /// Bring the HID device on root-hub `port` to the configured,
    /// boot-protocol state with a primed interrupt-IN ring (§4.3):
    /// port reset (when not yet enabled), Enable Slot, Address Device,
    /// `GET_DESCRIPTOR(device)`, Configure Endpoint,
    /// `SET_CONFIGURATION(1)`, and `SET_PROTOCOL(boot)`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Busy`] if a device was already enumerated on
    ///   this engine (one device per engine in this increment).
    /// * [`DriverError::OutOfRange`] if `port` is out of range.
    /// * [`DriverError::BadMagic`] if the device descriptor is forged.
    /// * [`DriverError::DeviceFault`] for every controller or device
    ///   protocol failure (fail closed, `AGENTS.md` §5.4).
    pub fn enumerate_hid(&mut self, port: u8) -> Result<DeviceDescriptor, DriverError> {
        if self.slot != 0 {
            return Err(DriverError::Busy);
        }
        let status = self.xhci.port_status(port)?;
        if !status.connected() {
            return Err(DriverError::DeviceFault);
        }
        let status = if status.enabled() {
            status
        } else {
            self.xhci.reset_port(port, self.budget)?
        };
        let max_packet = ep0_max_packet(status.speed())?;

        let event = self.command(Trb::new(TrbType::EnableSlot, 0, 0, 0))?;
        let slot = event.slot_id();
        if slot == 0 || slot > self.xhci.max_slots() {
            return Err(DriverError::DeviceFault);
        }
        self.slot = slot;

        // Address Device: input control (A0 | A1), slot context with
        // the default control endpoint only, and the EP0 context.
        self.write_input_ctx(0, &input_control_dwords(0b11))?;
        self.write_input_ctx(1, &slot_ctx_dwords(status.speed(), 1, port))?;
        self.write_input_ctx(
            1 + usize::from(DCI_CONTROL),
            &ep_ctx_dwords(
                EP_TYPE_CONTROL,
                max_packet,
                0,
                self.phys_of(self.layout.ep0_ring),
            ),
        )?;
        let output_ctx = self.phys_of(self.layout.output_ctx);
        self.dma.write(
            self.layout.dcbaa + usize::from(slot) * 8,
            &output_ctx.to_le_bytes(),
        )?;
        self.command(Trb::new(
            TrbType::AddressDevice,
            self.phys_of(self.layout.input_ctx),
            0,
            trb::control_slot(slot),
        ))?;

        let descriptor_len =
            u32::try_from(DeviceDescriptor::LEN).map_err(|_| DriverError::LengthOutOfRange)?;
        let transferred = self.control(setup_get_device_descriptor(0x12), descriptor_len)?;
        if transferred != descriptor_len {
            return Err(DriverError::DeviceFault);
        }
        let mut bytes = [0u8; DeviceDescriptor::LEN];
        self.dma.read(self.layout.ctrl_data, &mut bytes)?;
        let descriptor = DeviceDescriptor::decode(&bytes)?;

        // Read the configuration descriptor to discover the interface's
        // class triple and number, rather than assuming interface 0 /
        // boot keyboard (`AGENTS.md` §18.5 — the class is captured, not
        // fabricated). The whole control-data buffer is requested; the
        // device short-packets at the configuration's real length.
        let config_buf_len =
            u32::try_from(CTRL_DATA_LEN).map_err(|_| DriverError::LengthOutOfRange)?;
        let config_len_u16 =
            u16::try_from(CTRL_DATA_LEN).map_err(|_| DriverError::LengthOutOfRange)?;
        let transferred = self.control(
            setup_get_configuration_descriptor(config_len_u16),
            config_buf_len,
        )?;
        let transferred = usize::min(
            usize::try_from(transferred).map_err(|_| DriverError::DeviceFault)?,
            CTRL_DATA_LEN,
        );
        let mut config_bytes = [0u8; CTRL_DATA_LEN];
        self.dma.read(self.layout.ctrl_data, &mut config_bytes)?;
        let interface = InterfaceInfo::decode(&config_bytes[..transferred])?;

        // Configure the interrupt-IN endpoint (A0 | A3), raising the
        // slot's context entries to cover DCI 3.
        let report_len = u32::try_from(REPORT_LEN).map_err(|_| DriverError::LengthOutOfRange)?;
        self.write_input_ctx(0, &input_control_dwords(1 | (1 << DCI_INTERRUPT_IN)))?;
        self.write_input_ctx(
            1,
            &slot_ctx_dwords(status.speed(), u32::from(DCI_INTERRUPT_IN), port),
        )?;
        self.write_input_ctx(
            1 + usize::from(DCI_INTERRUPT_IN),
            &ep_ctx_dwords(
                EP_TYPE_INTERRUPT_IN,
                report_len,
                INT_EP_INTERVAL,
                self.phys_of(self.layout.int_ring),
            ),
        )?;
        self.command(Trb::new(
            TrbType::ConfigureEndpoint,
            self.phys_of(self.layout.input_ctx),
            0,
            trb::control_slot(slot),
        ))?;

        self.control(setup_set_configuration(interface.configuration_value), 0)?;
        self.control(setup_set_protocol_boot(interface.interface_number), 0)?;

        for _ in 0..PRIMED_REPORTS {
            self.arm_report()?;
        }
        self.xhci.ring_doorbell(slot, u32::from(DCI_INTERRUPT_IN))?;
        self.identity = Some(HidIdentity {
            vendor_id: descriptor.vendor_id,
            product_id: descriptor.product_id,
            interface_class: interface.class24,
        });
        Ok(descriptor)
    }

    /// Bring up the first root-hub port reporting a connected device.
    ///
    /// xHCI numbers root-hub ports from `1`; this scans `1..=max_ports`
    /// in order and enumerates the first that reports a device connected
    /// ([`Self::enumerate_hid`]). A composition that does not know which
    /// physical port a keyboard is plugged into (the boot keyboard case,
    /// `plans/PI.md` P10) uses this rather than guessing a port.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if no root-hub port reports a
    ///   connected device (an empty root hub — fail closed, never a
    ///   guessed port, `AGENTS.md` §5.4 / §2.9).
    /// * Any error of [`Self::enumerate_hid`] for the matched port, or a
    ///   faulting port-status read (the scan aborts fail-closed rather
    ///   than skipping a port whose status could not be read).
    ///
    /// # Capabilities
    ///
    /// None beyond those the controller's register window already holds.
    pub fn enumerate_first_connected(&mut self) -> Result<DeviceDescriptor, DriverError> {
        let max_ports = self.xhci.max_ports();
        for port in 1..=max_ports {
            if self.xhci.port_status(port)?.connected() {
                return self.enumerate_hid(port);
            }
        }
        Err(DriverError::NotFound)
    }

    /// Describe the enumerated HID device as a discovered child
    /// [`HwNode`] parented at `parent_id` and assigned `node_id`.
    ///
    /// The node carries one [`HwMatchKey::usb`] of the device's
    /// `vid:pid` and the 24-bit class of the interface this driver
    /// brought up — both read from the device during
    /// [`Self::enumerate_hid`], never assumed — so `devmgr` resolves an
    /// HID driver's signed bind table against it (`AGENTS.md` §18.3 /
    /// §18.5). Its [`HwDeviceClass`] is [`HwDeviceClass::Input`], the
    /// HID-class match key mirroring the PCI child node
    /// [`PciBus::describe_function`](rustos_abi::driver::pci::PciBus::describe_function)
    /// emits for the controller above it (`AGENTS.md` §2.2).
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if no device has been enumerated yet
    ///   (the identity is captured only on a successful
    ///   [`Self::enumerate_hid`]) — fail closed, never a fabricated node
    ///   (`AGENTS.md` §2.9 / §18.5).
    /// * [`DriverError::DeviceFault`] if the match key cannot be pushed.
    ///
    /// # Capabilities
    ///
    /// None — describing a node mints no resources (`AGENTS.md` §18.1:
    /// resources are minted at the load gate).
    pub fn describe_device(&self, parent_id: u32, node_id: u32) -> Result<HwNode, DriverError> {
        let identity = self.identity.ok_or(DriverError::NotFound)?;
        let mut node = HwNode::new(node_id, parent_id, HwDeviceClass::Input);
        node.push_match_key(HwMatchKey::usb(
            identity.vendor_id,
            identity.product_id,
            identity.interface_class,
        ))
        .map_err(|_| DriverError::DeviceFault)?;
        Ok(node)
    }
}

#[cfg(test)]
impl<H: XhciHost, M: DmaRegion> UsbDevice<H, M> {
    /// Test-only access to the register seam, so the crate's unit
    /// tests can drive and assert the mock controller's state.
    pub(crate) fn host_mut(&mut self) -> &mut H {
        &mut self.xhci.host
    }
}

impl<H: XhciHost, M: DmaRegion> ReportSource for UsbDevice<H, M> {
    fn next_report(&mut self, buf: &mut [u8]) -> Result<Option<usize>, DriverError> {
        if self.slot == 0 {
            // Not enumerated: there is no endpoint to drain.
            return Err(DriverError::DeviceFault);
        }
        // Bounded by the event segment: one pass can hold at most the
        // segment's TRBs, and `next_report` never blocks.
        for _ in 0..RING_TRBS {
            let Some(event) = self.poll_event()? else {
                return Ok(None);
            };
            match event.trb_type() {
                Ok(TrbType::PortStatusChange) => continue,
                Ok(TrbType::TransferEvent) => {}
                _ => return Err(DriverError::DeviceFault),
            }
            if event.slot_id() != self.slot || event.endpoint_id() != DCI_INTERRUPT_IN {
                return Err(DriverError::DeviceFault);
            }
            match event.completion_code() {
                Ok(CompletionCode::Success | CompletionCode::ShortPacket) => {}
                _ => return Err(DriverError::DeviceFault),
            }
            // Map the completed TRB back to its slot's report buffer,
            // validating every step of the controller's claim
            // (`AGENTS.md` §5.4).
            let ring_base = self.phys_of(self.layout.int_ring);
            let offset = event
                .parameter
                .checked_sub(ring_base)
                .ok_or(DriverError::DeviceFault)?;
            let trb_len = trb::TRB_LEN as u64;
            if offset % trb_len != 0 {
                return Err(DriverError::DeviceFault);
            }
            let slot = usize::try_from(offset / trb_len).map_err(|_| DriverError::DeviceFault)?;
            if slot >= RING_TRBS - 1 {
                return Err(DriverError::DeviceFault);
            }
            let residual =
                usize::try_from(event.transfer_residual()).map_err(|_| DriverError::DeviceFault)?;
            let len = REPORT_LEN
                .checked_sub(residual)
                .ok_or(DriverError::DeviceFault)?;
            if len == 0 || len > buf.len() {
                return Err(DriverError::DeviceFault);
            }
            self.dma
                .read(self.layout.report_bufs + slot * REPORT_LEN, &mut buf[..len])?;
            self.int_ring.retire_one()?;
            self.arm_report()?;
            self.xhci
                .ring_doorbell(self.slot, u32::from(DCI_INTERRUPT_IN))?;
            return Ok(Some(len));
        }
        Ok(None)
    }
}
