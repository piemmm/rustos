//! xHCI device enumeration (xHCI 1.2 §4.3) and the HID interrupt-IN
//! report path.
//!
//! [`UsbDevice`] drives one controller through the full bring-up of a
//! single attached HID device: port reset, Enable Slot, Address
//! Device, `GET_DESCRIPTOR(device)`, `SET_PROTOCOL(boot)`, Configure
//! Endpoint, and a primed interrupt-IN transfer ring. It then
//! implements the [`ReportSource`] seam from
//! `rustos_abi::driver::input`, so the host-controller driver serves reports
//! straight off the transfer ring over the URB transport to a class driver
//! (`drivers/input/usb_kbd`), whose `rustos_hid` decoders consume them.
//!
//! # Memory seam
//!
//! Every byte the controller shares with the driver lives in one
//! caller-provided region behind the [`DmaRegion`] trait — on metal a
//! capability-granted [`DmaSlab`], in host tests a plain shared
//! buffer — so the enumeration state machine is proven host-side
//! against the register-level mock plus an in-memory ring model. The engine performs every ring read/write
//! through the seam; the ring state machines themselves hold no
//! memory ([`ProducerRing`], [`EventRingCursor`]).

use rustos_abi::driver::dma::DmaSlab;
use rustos_abi::driver::input::ReportSource;
use rustos_abi::{Delay, DriverError, HwDeviceClass, HwMatchKey, HwNode};

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
/// scalable capacities: each ring only ever holds
/// the single in-flight command or control TD plus the primed
/// interrupt TRBs below.
pub const RING_TRBS: usize = 16;

/// Minimum TRBs in an xHCI event-ring segment.
pub const EVENT_RING_SEGMENT_MIN_TRBS: usize = 16;

const _: () = assert!(RING_TRBS >= EVENT_RING_SEGMENT_MIN_TRBS);

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

/// Device Context Index of the default control endpoint (§4.5.1). Also
/// the default for [`UsbDevice::int_dci`] before a HID interface's
/// interrupt endpoint is read from its descriptor.
const DCI_CONTROL: u8 = 1;

/// Hub power-on-good settle, in microseconds, before reading a downstream
/// port's connect status. A USB 2.0 hub reports `bPwrOn2PwrGood` in 2 ms
/// units and is commonly ≤ 100 ms (USB 2.0 §11.11); this fixed budget
/// covers the typical worst case rather than decoding the field. A fixed
/// protocol settle, not a scalable capacity.
const HUB_POWER_ON_GOOD_US: u32 = 100_000;

/// Reset-recovery settle, in microseconds, after a downstream-port
/// `SET_FEATURE(PORT_RESET)` before reading the port enabled and
/// addressing the device. USB 2.0 §7.1.7.5 requires ≥ 10 ms of reset
/// recovery; this conservative budget covers a slow hub.
const HUB_RESET_RECOVERY_US: u32 = 50_000;

/// Where each structure lives inside the caller's [`DmaRegion`].
///
/// All offsets are 64-byte aligned — the strictest alignment any of
/// the structures requires.
#[derive(Copy, Clone, Debug)]
struct Layout {
    dcbaa: usize,
    erst: usize,
    command_ring: usize,
    event_segment: usize,
    input_ctx: usize,
    output_ctx: usize,
    ep0_ring: usize,
    /// Second device slot's output device context, for a device
    /// enumerated *downstream* of an addressed hub ([`UsbDevice::
    /// enumerate_downstream_hid`]): the hub keeps [`Self::output_ctx`] /
    /// [`Self::ep0_ring`], the downstream device gets its own context and
    /// EP0 ring so both slots stay live in the DCBAA at once.
    output_ctx2: usize,
    /// Second device slot's default-control-endpoint transfer ring.
    ep0_ring2: usize,
    int_ring: usize,
    ctrl_data: usize,
    report_bufs: usize,
    /// Offset of the scratchpad buffer pointer array (xHCI §6.6): one
    /// 64-bit device-visible pointer per scratchpad buffer, the array
    /// `DCBAA[0]` points at. `0` when the controller needs no scratchpad.
    scratchpad_array: usize,
    /// Offset of the first scratchpad buffer page. Each buffer is one
    /// controller page and page-aligned. `0` when none.
    scratchpad_pages: usize,
    /// Number of scratchpad buffers reserved (`HCSPARAMS2` Max Scratchpad
    /// Buffers; the VL805 needs 31).
    scratchpad_count: usize,
    /// The controller page size each scratchpad buffer occupies.
    page_size: usize,
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
        // Invalidate the CPU's view of this range first, so a non-coherent
        // DMA master's writes (e.g. an event TRB the controller posted)
        // are read from memory rather than a stale cache line. A no-op on
        // a coherent interconnect / the mock host.
        DmaSlab::sync_range(self, offset, buf.len());
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
        // Clean this range to memory, so a non-coherent DMA master reads
        // the freshly published bytes (e.g. a command TRB) once the
        // doorbell is rung rather than stale memory. A no-op on a coherent
        // interconnect / the mock host.
        DmaSlab::sync_range(self, offset, bytes.len());
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
    ///   aligned.
    /// * [`DriverError::LengthOutOfRange`] if the region cannot hold
    ///   every structure.
    fn new(
        max_slots: u8,
        csz: bool,
        region_len: usize,
        phys: u64,
        scratchpad_count: u32,
        page_size: usize,
    ) -> Result<Self, DriverError> {
        if phys == 0 || phys % 64 != 0 {
            return Err(DriverError::OutOfRange);
        }
        let scratchpad_count = scratchpad_count as usize;
        // A controller that needs scratchpad must report a page size, and
        // the region base must be page-aligned so each buffer lands on a
        // page boundary in the device address space (xHCI §4.20 / §6.6).
        // Fail closed otherwise.
        if scratchpad_count > 0 && (page_size == 0 || phys % page_size as u64 != 0) {
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
        let output_ctx2 = take(OUTPUT_CONTEXTS * ctx_size);
        let ep0_ring2 = take(RING_TRBS * trb::TRB_LEN);
        let int_ring = take(RING_TRBS * trb::TRB_LEN);
        let ctrl_data = take(CTRL_DATA_LEN);
        let report_bufs = take(RING_TRBS * REPORT_LEN);
        let (scratchpad_array, scratchpad_pages) = if scratchpad_count > 0 {
            let array = take(scratchpad_count * 8);
            // The buffer pages must be page-aligned, not merely 64-aligned.
            next = next.next_multiple_of(page_size);
            let pages = next;
            next = next
                .checked_add(scratchpad_count * page_size)
                .ok_or(DriverError::LengthOutOfRange)?;
            (array, pages)
        } else {
            (0, 0)
        };
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
            output_ctx2,
            ep0_ring2,
            int_ring,
            ctrl_data,
            report_bufs,
            scratchpad_array,
            scratchpad_pages,
            scratchpad_count,
            page_size,
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
/// (USB2 §5.5.3, USB3 §9.6.6).
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

/// `bDescriptorType` of an endpoint descriptor (USB 2.0 §9.4 Table 9-5).
const DESC_TYPE_ENDPOINT: u8 = 0x05;

/// Byte length of an endpoint descriptor (USB 2.0 §9.6.6).
const ENDPOINT_DESCRIPTOR_LEN: usize = 7;

/// `bmAttributes` transfer-type mask and the Interrupt transfer type
/// (USB 2.0 §9.6.6 Table 9-13).
const ENDPOINT_ATTR_TYPE_MASK: u8 = 0x03;
const ENDPOINT_ATTR_INTERRUPT: u8 = 0x03;

/// `bEndpointAddress` direction bit (USB 2.0 §9.6.6): set for an IN
/// endpoint.
const ENDPOINT_ADDR_DIR_IN: u8 = 0x80;

/// `bEndpointAddress` endpoint-number mask (USB 2.0 §9.6.6).
const ENDPOINT_ADDR_NUMBER_MASK: u8 = 0x0F;

/// `wMaxPacketSize` packet-size mask (USB 2.0 §9.6.6 bits 0:10).
const ENDPOINT_MAX_PACKET_MASK: u16 = 0x07FF;

/// `bInterfaceClass` of a Human Interface Device (USB HID 1.11 §4.1).
/// The HID-specific `SET_PROTOCOL` class request is only sent to an
/// interface of this class; a non-HID interface (e.g. a hub, class
/// `0x09`) STALLs it, which in xHCI **halts** the control endpoint and
/// would break a following EP0 transfer (`UsbDevice::enumerate_hid`).
/// Held as the top byte of the 24-bit class triple ([`InterfaceInfo`]).
const INTERFACE_CLASS_HID: u32 = 0x03;

/// `bDeviceClass` of a USB hub (USB 2.0 §11.23.1). The Pi 4B's onboard
/// `2109:3431` VIA Labs hub reports this, so the keyboard plugged into a
/// USB-A port is a device *downstream* of the hub, not on a root-hub
/// port — reaching it requires walking the hub (`plans/PI.md`).
const DEVICE_CLASS_HUB: u8 = 0x09;

/// `bDescriptorType` of a USB 2.0 hub class descriptor (USB 2.0
/// §11.23.2.1), requested with a class `GET_DESCRIPTOR`.
const DESC_TYPE_HUB: u8 = 0x29;

/// Hub class port feature selector `PORT_POWER` (USB 2.0 §11.24.2,
/// Table 11-17): a port-power-controlled hub reports a downstream port
/// disconnected until software sets this.
const PORT_FEATURE_POWER: u8 = 8;

/// Hub class port feature selector `PORT_RESET` (USB 2.0 §11.24.2,
/// Table 11-17): resetting a downstream port enables it and lets the
/// hub establish the device's speed (and, for a full/low-speed device,
/// its transaction translator) before the device is addressed.
const PORT_FEATURE_RESET: u8 = 4;

/// `wPortStatus` bit: Current Connect Status (USB 2.0 §11.24.2.7.1).
const PORT_STATUS_CONNECT: u16 = 1 << 0;

/// `wPortStatus` bit: Port Enabled (USB 2.0 §11.24.2.7.1): set by the
/// hub once a port reset completes, the gate the downstream device must
/// pass before it can be addressed.
const PORT_STATUS_ENABLE: u16 = 1 << 1;

/// `wPortStatus` bit: Low-Speed Device Attached (USB 2.0 §11.24.2.7.1).
const PORT_STATUS_LOW_SPEED: u16 = 1 << 9;

/// `wPortStatus` bit: High-Speed Device Attached (USB 2.0 §11.24.2.7.1).
const PORT_STATUS_HIGH_SPEED: u16 = 1 << 10;

/// xHCI protocol speed ID for a full-speed device (§7.2.1 default speed
/// IDs): the speed of the Pi 4B's keyboard behind the high-speed hub.
const SPEED_FULL: u8 = 1;

/// xHCI protocol speed ID for a low-speed device (§7.2.1).
const SPEED_LOW: u8 = 2;

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
    ///   zero configurations — a forged or corrupt reply.
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

    /// Whether this device descriptor describes a USB hub (USB 2.0
    /// §11.23.1).
    ///
    /// The Pi 4B's onboard `2109:3431` hub reports `bDeviceClass = 0x09`;
    /// a keyboard plugged into a USB-A port enumerates *downstream* of
    /// it, so the bring-up must walk the hub's ports rather than treat
    /// the enumerated device as the keyboard.
    #[must_use]
    pub const fn is_hub(&self) -> bool {
        self.device_class == DEVICE_CLASS_HUB
    }
}

/// The 8-byte SETUP payload of `SET_CONFIGURATION(value)` (USB 2.0
/// §9.4.7) — class requests like `SET_PROTOCOL` are only defined on a
/// configured device.
const fn setup_set_configuration(value: u8) -> [u8; 8] {
    [0x00, 0x09, value, 0x00, 0x00, 0x00, 0x00, 0x00]
}

/// The 8-byte SETUP payload of the class `GET_DESCRIPTOR(hub)` request
/// (USB 2.0 §11.24.2.5): `bmRequestType = 0xA0` (device-to-host, class,
/// device), descriptor type [`DESC_TYPE_HUB`] in the high byte of
/// `wValue`, for `len` bytes.
const fn setup_get_hub_descriptor(len: u16) -> [u8; 8] {
    let l = len.to_le_bytes();
    [0xA0, 0x06, 0x00, DESC_TYPE_HUB, 0x00, 0x00, l[0], l[1]]
}

/// The 8-byte SETUP payload of `SET_FEATURE(feature)` on a downstream
/// hub `port` (USB 2.0 §11.24.2.13): `bmRequestType = 0x23`
/// (host-to-device, class, other), `feature` in `wValue`, the 1-based
/// `port` in `wIndex`, no data stage.
const fn setup_set_port_feature(feature: u8, port: u8) -> [u8; 8] {
    [0x23, 0x03, feature, 0x00, port, 0x00, 0x00, 0x00]
}

/// The 8-byte SETUP payload of `GET_STATUS` on a downstream hub `port`
/// (USB 2.0 §11.24.2.7): `bmRequestType = 0xA3` (device-to-host, class,
/// other), the 1-based `port` in `wIndex`, a 4-byte
/// `wPortStatus`/`wPortChange` IN data stage.
const fn setup_get_port_status(port: u8) -> [u8; 8] {
    [0xA3, 0x00, 0x00, 0x00, port, 0x00, 0x04, 0x00]
}

/// Whether a hub port's 16-bit `wPortStatus` reports a connected
/// downstream device (USB 2.0 §11.24.2.7.1, Current Connect Status).
#[must_use]
pub const fn hub_port_connected(status: u16) -> bool {
    status & PORT_STATUS_CONNECT != 0
}

/// Whether a hub port's 16-bit `wPortStatus` reports the port enabled
/// (USB 2.0 §11.24.2.7.1) — set by the hub once a port reset completes.
#[must_use]
pub const fn hub_port_enabled(status: u16) -> bool {
    status & PORT_STATUS_ENABLE != 0
}

/// Whether `speed` (an xHCI protocol speed ID, [`hub_port_speed`]) is a
/// full- or low-speed device, which behind a high-speed hub must route
/// through that hub's transaction translator (xHCI §6.2.2 TT fields).
const fn speed_needs_tt(speed: u8) -> bool {
    speed == SPEED_FULL || speed == SPEED_LOW
}

/// Map a hub port's `wPortStatus` speed bits to an xHCI protocol speed
/// ID (USB 2.0 §11.24.2.7.1): Low-Speed → 2, High-Speed → 3, neither →
/// 1 (full speed). Only meaningful when [`hub_port_connected`].
#[must_use]
pub const fn hub_port_speed(status: u16) -> u8 {
    if status & PORT_STATUS_LOW_SPEED != 0 {
        2
    } else if status & PORT_STATUS_HIGH_SPEED != 0 {
        3
    } else {
        1
    }
}

/// The configuration- and first-interface-descriptor fields this driver
/// needs (USB 2.0 §9.6.3 / §9.6.5), decoded fail-closed from the
/// `GET_DESCRIPTOR(configuration)` bytes. The interface class is read from
/// the device, never assumed, so the emitted hardware-tree child node
/// carries the honest class.
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
    /// Device Context Index of the interface's interrupt-IN endpoint
    /// (§4.5.1: `2 * endpoint_number + 1`), read from its endpoint
    /// descriptor rather than assumed (a keyboard need not use endpoint 1).
    /// The default control-endpoint DCI (`1`) for a non-HID interface.
    pub int_dci: u8,
    /// `wMaxPacketSize` (bits 0:10) of the interrupt-IN endpoint, the
    /// endpoint-context Max Packet Size and Max ESIT Payload. `0` for a
    /// non-HID interface.
    pub int_max_packet: u16,
    /// `bInterval` of the interrupt-IN endpoint as the device reported
    /// it (speed-dependent units, decoded by `interrupt_interval`).
    /// `0` for a non-HID interface.
    pub int_b_interval: u8,
}

impl InterfaceInfo {
    /// Byte length of a configuration descriptor header (USB 2.0
    /// §9.6.3) and of an interface descriptor (§9.6.5).
    const CONFIG_HEADER_LEN: usize = 9;
    const INTERFACE_LEN: usize = 9;

    /// Decode the `GET_DESCRIPTOR(configuration)` bytes into the
    /// configuration value, the **first** interface's number and class
    /// triple, and that interface's first interrupt-IN endpoint (DCI, max
    /// packet size, `bInterval`). Walks the concatenated descriptors by
    /// each `bLength` (the endpoint is read, never assumed).
    ///
    /// # Errors
    ///
    /// [`DriverError::BadMagic`] for a non-configuration leading
    /// descriptor, a length running off the buffer or below its minimum,
    /// no interface descriptor, or a HID interface with no interrupt-IN
    /// endpoint — a forged or corrupt reply.
    pub fn decode(buf: &[u8]) -> Result<Self, DriverError> {
        if buf.len() < Self::CONFIG_HEADER_LEN
            || usize::from(buf[0]) < Self::CONFIG_HEADER_LEN
            || buf[1] != DESC_TYPE_CONFIGURATION
        {
            return Err(DriverError::BadMagic);
        }
        let configuration_value = buf[5];
        let mut offset = usize::from(buf[0]);
        let mut interface: Option<(u8, u32)> = None;
        let mut int_endpoint: Option<(u8, u16, u8)> = None;
        while offset + 2 <= buf.len() {
            let length = usize::from(buf[offset]);
            let end = offset.checked_add(length).ok_or(DriverError::BadMagic)?;
            if length < 2 || end > buf.len() {
                return Err(DriverError::BadMagic);
            }
            match buf[offset + 1] {
                DESC_TYPE_INTERFACE => {
                    // Only the first interface is matched; a second one
                    // ends the search so its endpoints are never mistaken
                    // for the matched interface's (USB 2.0 §9.4.3).
                    if interface.is_some() {
                        break;
                    }
                    if length < Self::INTERFACE_LEN {
                        return Err(DriverError::BadMagic);
                    }
                    interface = Some((
                        buf[offset + 2],
                        (u32::from(buf[offset + 5]) << 16)
                            | (u32::from(buf[offset + 6]) << 8)
                            | u32::from(buf[offset + 7]),
                    ));
                }
                DESC_TYPE_ENDPOINT if interface.is_some() && int_endpoint.is_none() => {
                    if length < ENDPOINT_DESCRIPTOR_LEN {
                        return Err(DriverError::BadMagic);
                    }
                    let address = buf[offset + 2];
                    let attributes = buf[offset + 3];
                    if attributes & ENDPOINT_ATTR_TYPE_MASK == ENDPOINT_ATTR_INTERRUPT
                        && address & ENDPOINT_ADDR_DIR_IN != 0
                    {
                        let endpoint_number = address & ENDPOINT_ADDR_NUMBER_MASK;
                        let dci = endpoint_number * 2 + 1;
                        let max_packet = u16::from_le_bytes([buf[offset + 4], buf[offset + 5]])
                            & ENDPOINT_MAX_PACKET_MASK;
                        int_endpoint = Some((dci, max_packet, buf[offset + 6]));
                    }
                }
                _ => {}
            }
            offset = end;
        }
        let (interface_number, class24) = interface.ok_or(DriverError::BadMagic)?;
        let is_hid = class24 >> 16 == INTERFACE_CLASS_HID;
        // A HID interface without an interrupt-IN endpoint is a forged or
        // corrupt reply: there is nothing to poll for reports (USB HID
        // 1.11 §4.4). A non-HID interface (e.g. a hub)
        // carries no endpoint this engine services.
        let (int_dci, int_max_packet, int_b_interval) = match int_endpoint {
            Some(endpoint) => endpoint,
            None if is_hid => return Err(DriverError::BadMagic),
            None => (DCI_CONTROL, 0, 0),
        };
        Ok(Self {
            configuration_value,
            interface_number,
            class24,
            int_dci,
            int_max_packet,
            int_b_interval,
        })
    }

    /// Whether the matched interface is a Human Interface Device (USB
    /// HID 1.11 §4.1), i.e. `bInterfaceClass == 0x03`.
    ///
    /// The HID-specific `SET_PROTOCOL(boot)` request is only issued to a
    /// HID interface: a non-HID interface (a hub reports interface class
    /// `0x09`) STALLs it, halting the xHCI control endpoint and breaking
    /// any subsequent EP0 transfer such as the hub-descriptor read.
    #[must_use]
    pub const fn is_hid(&self) -> bool {
        self.class24 >> 16 == INTERFACE_CLASS_HID
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

/// Encode the xHCI endpoint-context Interval (§6.2.3.6, Table 6-12) for
/// an interrupt endpoint reporting `b_interval` at protocol `speed`.
///
/// High-/SuperSpeed `bInterval` is already a `2^(n-1)·125µs` exponent, so
/// the context Interval is `bInterval - 1` (clamped 0..=15). Full-/low-speed
/// `bInterval` is in frames (1 ms): converted to 125µs microframes (×8)
/// and reduced to its log2 exponent, clamped to the 3..=10 the periodic
/// scheduler accepts. Derived per-endpoint, not hard-coded.
fn interrupt_interval(speed: u8, b_interval: u8) -> u32 {
    let b_interval = b_interval.max(1);
    match speed {
        SPEED_FULL | SPEED_LOW => {
            let microframes = u32::from(b_interval).saturating_mul(8);
            let exponent = u32::BITS - 1 - microframes.leading_zeros();
            exponent.clamp(3, 10)
        }
        _ => u32::from(b_interval - 1).min(15),
    }
}

/// Input control context dwords: dword 1 carries the Add Context
/// flags (`A0` = slot context, `A(dci)` = that endpoint, §6.2.5.1).
fn input_control_dwords(add_flags: u32) -> [u32; CTX_DWORDS] {
    let mut dwords = [0; CTX_DWORDS];
    dwords[1] = add_flags;
    dwords
}

/// The topology fields of a device's slot context (xHCI §6.2.2) that
/// stay constant across both Address Device and Configure Endpoint for
/// one device: its protocol speed ID, the root-hub port it is reached
/// through, and — for a device *downstream* of a hub — the Route String
/// and transaction-translator (TT) coordinates.
///
/// A device directly on a root-hub port uses [`SlotCtxBase::root`]
/// (route string `0`, no TT). A full/low-speed device behind a
/// high-speed hub additionally names that hub's slot and downstream
/// port as its TT, so the controller splits its transactions
/// (`speed_needs_tt`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct SlotCtxBase {
    /// xHCI protocol speed ID ([`hub_port_speed`] / [`ep0_max_packet`]).
    speed: u8,
    /// The 1-based root-hub port the device is reached through (the hub's
    /// own root port for a downstream device).
    root_port: u8,
    /// Route String: the chain of downstream hub ports from the
    /// root to the device, four bits per tier. `0` for a root-port
    /// device.
    route_string: u32,
    /// TT Hub Slot ID (§6.2.2): the slot of the high-speed hub providing
    /// the transaction translator, or `0` when the device needs none.
    tt_hub_slot: u8,
    /// TT Port Number (§6.2.2): the hub's 1-based downstream port the
    /// device is attached to, or `0` when the device needs no TT.
    tt_port: u8,
}

impl SlotCtxBase {
    /// A device directly on root-hub `port` at `speed`: no route string,
    /// no transaction translator.
    fn root(speed: u8, port: u8) -> Self {
        Self {
            speed,
            root_port: port,
            route_string: 0,
            tt_hub_slot: 0,
            tt_port: 0,
        }
    }
}

/// Slot context dword 0 **Hub** bit (§6.2.2): the device on this slot is
/// a USB hub. The controller routes packets to — and, with the TT
/// fields, splits the transactions of — devices addressed downstream of
/// it only when this is set, so a keyboard behind the hub never receives
/// its interrupt transfers otherwise.
const SLOT_CTX_HUB: u32 = 1 << 26;
/// Slot context dword 0 **Multi-TT** bit (§6.2.2): the hub exposes one
/// transaction translator per port. The Pi 4B's onboard VIA hub is
/// single-TT, so this stays clear.
const SLOT_CTX_MTT: u32 = 1 << 25;
/// Slot context dword 1 **Number of Ports** field shift (§6.2.2): a
/// hub's downstream port count, used by the controller for periodic
/// transfer scheduling.
const SLOT_CTX_NUM_PORTS_SHIFT: u32 = 24;
/// Slot context dword 2 **TT Think Time** field shift and mask
/// (§6.2.2): the inter-transaction gap the hub's TT needs, in FS bit
/// times, copied from the hub descriptor's `wHubCharacteristics`.
const SLOT_CTX_TTT_SHIFT: u32 = 16;
const SLOT_CTX_TTT_MASK: u32 = 0b11 << SLOT_CTX_TTT_SHIFT;

/// Slot context dwords (§6.2.2): the Route String and protocol speed ID
/// (dword 0), context entries (the highest DCI in use) and the root-hub
/// port number (dword 1), and the transaction-translator coordinates
/// (dword 2) for a full/low-speed device behind a high-speed hub.
fn slot_ctx_dwords(base: SlotCtxBase, context_entries: u32) -> [u32; CTX_DWORDS] {
    let mut dwords = [0; CTX_DWORDS];
    dwords[0] =
        (base.route_string & 0x000F_FFFF) | (u32::from(base.speed) << 20) | (context_entries << 27);
    dwords[1] = u32::from(base.root_port) << 16;
    dwords[2] = u32::from(base.tt_hub_slot) | (u32::from(base.tt_port) << 8);
    dwords
}

/// Endpoint context dwords (§6.2.3): error count 3, endpoint type, max
/// packet size, service interval, the transfer-ring dequeue pointer with
/// Dequeue Cycle State 1, average TRB length, and — for a periodic
/// endpoint — the Max ESIT Payload (dword 4 bits 16:31). A periodic
/// endpoint **must** carry a non-zero Max ESIT Payload or the scheduler
/// reserves no bandwidth and no transfer runs (§4.14.2); a control/bulk
/// endpoint (Interval `0`) leaves it reserved-zero. For a boot HID
/// endpoint it is the max packet size.
fn ep_ctx_dwords(ep_type: u32, max_packet: u32, interval: u32, ring: u64) -> [u32; CTX_DWORDS] {
    let mut dwords = [0; CTX_DWORDS];
    dwords[0] = interval << 16;
    dwords[1] = (3 << 1) | (ep_type << 3) | (max_packet << 16);
    let dequeue = ring | 1;
    dwords[2] = crate::low_dword(dequeue);
    dwords[3] = crate::high_dword(dequeue);
    let max_esit_payload = if interval != 0 { max_packet } else { 0 };
    dwords[4] = max_packet | (max_esit_payload << 16);
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

/// The step [`UsbDevice::enumerate_hid`] last entered, a breadcrumb so a
/// capture can localise which xHCI operation a coarse
/// [`DriverError::DeviceFault`] came from. Stays at [`EnumStage::Scan`]
/// until a connected port enters enumeration, so an empty-hub
/// [`DriverError::NotFound`] stays distinguishable. Variants follow the
/// enumeration sequence.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum EnumStage {
    /// Before (or between) any per-device step: scanning the root hub.
    Scan = 0,
    /// Resetting a connected-but-not-yet-enabled port.
    PortReset = 1,
    /// Enable Slot command (§6.4.3.2).
    EnableSlot = 2,
    /// Address Device command (§6.4.3.4).
    AddressDevice = 3,
    /// `GET_DESCRIPTOR(device)` control transfer (§9.4.3).
    GetDeviceDescriptor = 4,
    /// `GET_DESCRIPTOR(configuration)` control transfer (§9.4.3).
    GetConfigDescriptor = 5,
    /// Configure Endpoint command (§6.4.3.5).
    ConfigureEndpoint = 6,
    /// `SET_CONFIGURATION` control transfer (§9.4.7).
    SetConfiguration = 7,
    /// HID `SET_PROTOCOL(boot)` class request (HID 1.11 §7.2.6).
    SetProtocol = 8,
    /// Priming the interrupt-IN ring with report TRBs.
    ArmReport = 9,
    /// Enumeration completed: the device is configured and primed.
    Configured = 10,
}

impl EnumStage {
    /// Raw discriminant, for an allocation-free diagnostic log.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// [`UsbDevice::last_reject`] reason: the wait succeeded, or none has
/// run yet.
const REJECT_NONE: u8 = 0;
/// [`UsbDevice::last_reject`] reason: an event of a TRB-type the
/// consumer does not handle (e.g. an asynchronous controller event).
const REJECT_UNEXPECTED_TYPE: u8 = 1;
/// [`UsbDevice::last_reject`] reason: a completion for a TRB this
/// transfer did not enqueue.
const REJECT_ADDRESS_MISMATCH: u8 = 2;
/// [`UsbDevice::last_reject`] reason: an event carrying a completion
/// code the driver does not model.
const REJECT_UNDECODABLE_CODE: u8 = 3;
/// [`UsbDevice::last_reject`] reason: the poll budget elapsed with no
/// event observed — a genuine timeout.
const REJECT_BUDGET_TIMEOUT: u8 = 4;

/// One enumerated HID device on a started xHCI controller.
///
/// [`UsbDevice::start`] lays the DMA structures out, programs them
/// through [`Xhci::start`], and leaves the controller running.
/// [`UsbDevice::enumerate_hid`] then brings the device on `port` to
/// the configured state (boot protocol when the device accepts it)
/// with a primed interrupt-IN ring, after which
/// [`ReportSource::next_report`] drains reports the host-controller driver
/// serves over the URB transport to the `drivers/input/usb_kbd` decoders.
pub struct UsbDevice<H: XhciHost, M: DmaRegion> {
    xhci: Xhci<H>,
    dma: M,
    layout: Layout,
    command_ring: ProducerRing,
    /// The default-control-endpoint transfer ring of the **currently
    /// active** device slot ([`Self::slot`]). Initially the only device's
    /// ring; rebound to the second region by
    /// [`Self::rebind_to_downstream_region`] when a keyboard is addressed
    /// downstream of an enumerated hub, so the hub's ring stays intact in
    /// the DCBAA while EP0 transfers target the keyboard.
    ep0_ring: ProducerRing,
    /// Region offset of [`Self::ep0_ring`] (the active slot's EP0 ring),
    /// for publishing TRBs into it. Either [`Layout::ep0_ring`] (the
    /// first device) or [`Layout::ep0_ring2`] (a downstream device).
    ep0_ring_off: usize,
    /// Region offset of the active slot's output device context, written
    /// into its DCBAA entry by [`Self::address_device`]. Either
    /// [`Layout::output_ctx`] or [`Layout::output_ctx2`].
    output_ctx_off: usize,
    /// The 1-based root-hub port the first device ([`Self::slot`] before
    /// any downstream addressing) was enumerated on, so a device
    /// addressed downstream of a hub reuses the hub's root port in its
    /// slot context (xHCI §6.2.2). `0` until the first enumeration.
    root_port: u8,
    int_ring: ProducerRing,
    event_cursor: EventRingCursor,
    budget: u32,
    slot: u8,
    identity: Option<HidIdentity>,
    /// Device Context Index of the active slot's interrupt-IN endpoint,
    /// read from its endpoint descriptor during enumeration (§4.5.1).
    /// [`DCI_CONTROL`] until a HID interface is configured; the
    /// doorbell and the [`ReportSource::next_report`] endpoint-id
    /// check both use it, so a keyboard whose interrupt endpoint is
    /// not endpoint 1 is still serviced.
    int_dci: u8,
    /// The last enumeration step [`Self::enumerate_hid`] entered, for a
    /// one-shot fault-localising diagnostic ([`Self::enum_stage`]).
    stage: EnumStage,
    /// Raw completion code of the most recent event TRB
    /// [`Self::command`] / [`Self::control`] observed (`0` = none seen
    /// since the current operation began — i.e. a timeout), for the
    /// same diagnostic ([`Self::last_completion_code`]).
    last_completion: u8,
    /// Raw TRB-type of the most recent event [`Self::await_event_for`]
    /// observed since the current operation began (`0` = none), for
    /// [`Self::last_event_type`].
    last_event_type: u8,
    /// Why the most recent [`Self::await_event_for`] failed, for
    /// [`Self::last_reject_reason`]: `0` none (succeeded or not yet
    /// run), `1` an event of a TRB-type the consumer does not handle,
    /// `2` a completion for a TRB this transfer did not enqueue,
    /// `3` an event carrying an undecodable completion code, `4` the
    /// poll budget elapsed with no event (a genuine timeout).
    last_reject: u8,
}

impl<H: XhciHost, M: DmaRegion> UsbDevice<H, M> {
    /// Lay out and zero the DMA structures inside `dma`, program them,
    /// and start the controller.
    ///
    /// `budget` bounds every wait this engine performs (register
    /// polls and event-ring polls), failing closed on a stuck
    /// controller.
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
        let layout = Layout::new(
            xhci.max_slots(),
            xhci.csz(),
            dma.len(),
            dma.phys(),
            xhci.max_scratchpad_buffers(),
            xhci.page_size(),
        )?;

        let zeros = [0u8; 64];
        let mut offset = 0;
        while offset < layout.total {
            let chunk = (layout.total - offset).min(zeros.len());
            dma.write(offset, &zeros[..chunk])?;
            offset += chunk;
        }

        // The single event ring segment table entry: segment base and
        // size in TRBs.
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

        // Reserve the controller's scratchpad buffers (xHCI §4.20): fill
        // the scratchpad pointer array with the device-visible base of
        // each page-aligned buffer, then point `DCBAA[0]` at that array.
        // The VL805 reports 31 buffers and cannot execute a single command
        // without them — the very first Enable Slot produces no completion
        // event (the Pi 4 `stage=2 completion=0` metal symptom). A
        // controller reporting `0` skips this entirely.
        if layout.scratchpad_count > 0 {
            for index in 0..layout.scratchpad_count {
                let page = dma.phys() + (layout.scratchpad_pages + index * layout.page_size) as u64;
                dma.write(layout.scratchpad_array + index * 8, &page.to_le_bytes())?;
            }
            let array = dma.phys() + layout.scratchpad_array as u64;
            dma.write(layout.dcbaa, &array.to_le_bytes())?;
        }

        xhci.start(
            &DmaProgram {
                dcbaap: dma.phys() + layout.dcbaa as u64,
                command_ring: dma.phys() + layout.command_ring as u64,
                erst: dma.phys() + layout.erst as u64,
                event_segment: event_phys,
            },
            budget,
        )?;

        let ep0_ring_off = layout.ep0_ring;
        let output_ctx_off = layout.output_ctx;
        Ok(Self {
            xhci,
            dma,
            layout,
            command_ring,
            ep0_ring,
            ep0_ring_off,
            output_ctx_off,
            root_port: 0,
            int_ring,
            event_cursor,
            budget,
            slot: 0,
            identity: None,
            int_dci: DCI_CONTROL,
            stage: EnumStage::Scan,
            last_completion: 0,
            last_event_type: 0,
            last_reject: 0,
        })
    }

    /// The slot ID the enumerated device occupies (`0` before
    /// [`Self::enumerate_hid`] succeeds).
    #[must_use]
    pub const fn slot(&self) -> u8 {
        self.slot
    }

    /// The root-hub port the enumerated device is reached through (`0` before
    /// enumeration assigns it).
    ///
    /// A host-controller driver reads the matching
    /// [`Self::root_port_status_raw`] to watch for the device disconnecting
    /// (the `CCS` connect bit clearing), so it can retract the interface node
    /// it published. For a device behind the onboard hub this is the *hub's*
    /// root port; detecting a downstream-of-hub disconnect needs the hub's own
    /// per-port status and is a finer-grained watch.
    #[must_use]
    pub const fn root_port(&self) -> u8 {
        self.root_port
    }

    /// Enable interrupt generation on the controller's interrupter so a
    /// posted transfer event asserts the device's interrupt (the MSI write,
    /// on the PCIe VL805) rather than only landing on the event ring.
    ///
    /// A driver that services this device interrupt-driven calls it once,
    /// after enumeration and after its interrupt line has been routed to it,
    /// then parks on `irq_wait` instead of busy-polling [`ReportSource::
    /// next_report`]. A poll-only consumer never calls it. Delegates to
    /// [`Xhci::enable_interrupter`].
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if the register window rejects a write.
    pub fn enable_interrupter(&mut self) -> Result<(), DriverError> {
        self.xhci.enable_interrupter()
    }

    /// Acknowledge the controller interrupter's pending interrupt
    /// (`IMAN.IP`), keeping it armed (xHCI §4.17.5).
    ///
    /// Called at the **start** of servicing a delivered interrupt — before
    /// the reports are drained through [`ReportSource::next_report`] — so a
    /// completion the controller posts during the drain re-asserts the
    /// interrupt and is not lost. Delegates to
    /// [`Xhci::acknowledge_interrupt`].
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if the register window rejects the write.
    pub fn acknowledge_interrupt(&mut self) -> Result<(), DriverError> {
        self.xhci.acknowledge_interrupt()
    }

    /// Device-visible address of byte `offset` within the region.
    fn phys_of(&self, offset: usize) -> u64 {
        self.dma.phys() + offset as u64
    }

    /// Consume the next controller event, advancing `ERDP` when one
    /// was taken.
    fn poll_event(&mut self) -> Result<Option<Trb>, DriverError> {
        // First snapshot: decide *whether* the controller has produced the
        // event at the dequeue point, by its cycle bit alone.
        let trbs = self.read_event_segment()?;
        if !self.event_cursor.owned(&trbs)? {
            return Ok(None);
        }
        // An event is owned. The controller writes the entry body before it
        // sets the cycle bit; on the device-shared Normal-Non-Cacheable DMA
        // region those two writes are not ordered for this PE without a
        // barrier, so the first snapshot's body bytes may predate the cycle
        // bit (a torn read pairing a fresh cycle with a stale TRB pointer —
        // the metal `REJECT_ADDRESS_MISMATCH` this fixes). Order the body read
        // after the cycle observation, then re-read and consume (see `rustos_dma_barrier`).
        rustos_dma_barrier::dma_rmb();
        let trbs = self.read_event_segment()?;
        let event = self.event_cursor.pop(&trbs)?;
        if event.is_some() {
            let erdp = self.phys_of(self.layout.event_segment)
                + (self.event_cursor.dequeue_index() * trb::TRB_LEN) as u64;
            self.xhci.ack_event(erdp)?;
        }
        Ok(event)
    }

    /// Read the whole single-segment event ring out of DMA into TRBs.
    fn read_event_segment(&mut self) -> Result<[Trb; RING_TRBS], DriverError> {
        let mut bytes = [0u8; RING_TRBS * trb::TRB_LEN];
        self.dma.read(self.layout.event_segment, &mut bytes)?;
        let mut trbs = [Trb::ZERO; RING_TRBS];
        for (index, slot) in trbs.iter_mut().enumerate() {
            let mut image = [0u8; trb::TRB_LEN];
            image.copy_from_slice(&bytes[index * trb::TRB_LEN..(index + 1) * trb::TRB_LEN]);
            *slot = Trb::from_bytes(image);
        }
        Ok(trbs)
    }

    /// Reset the per-transfer event diagnostics before a fresh command
    /// or control transfer, so [`Self::last_completion_code`],
    /// [`Self::last_event_type`], and [`Self::last_reject_reason`]
    /// describe only that transfer.
    fn reset_event_diagnostics(&mut self) {
        self.last_completion = 0;
        self.last_event_type = 0;
        self.last_reject = REJECT_NONE;
    }

    /// Wait for a completion event for one of `addresses` (the TRBs in
    /// flight), skipping informational port-status-change events.
    ///
    /// A completion for a TRB never issued, an undecodable completion
    /// code, or an unexpected event type is a controller fault,
    /// surfaced rather than absorbed. Every reject
    /// path records *why* it failed in [`Self::last_reject`] and the
    /// observed event's raw TRB-type in [`Self::last_event_type`], so a
    /// metal capture can tell an unexpected asynchronous event from a
    /// genuine timeout — the `completion_hex` alone cannot.
    fn await_event_for(&mut self, addresses: &[u64]) -> Result<Trb, DriverError> {
        for _ in 0..self.budget {
            let Some(event) = self.poll_event()? else {
                continue;
            };
            self.last_event_type = event.trb_type_raw();
            match event.trb_type() {
                Ok(TrbType::PortStatusChange) => {}
                Ok(TrbType::CommandCompletion | TrbType::TransferEvent) => {
                    // Record the raw completion code of *every* command/
                    // transfer event the moment it is observed — before
                    // the address match and before the fail-closed
                    // `completion_code()` decode below. A rejection here
                    // (an event for a TRB we did not enqueue, or a code
                    // this driver does not model) otherwise returned
                    // before the caller could capture the code, leaving
                    // `last_completion_code()` reading `0` ("no event")
                    // and conflating a genuine timeout with a real-but-
                    // rejected completion. Capturing it here keeps the
                    // diagnostic truthful.
                    self.last_completion = event.completion_code_raw();
                    if !addresses.contains(&event.parameter) {
                        self.last_reject = REJECT_ADDRESS_MISMATCH;
                        return Err(DriverError::DeviceFault);
                    }
                    if event.completion_code().is_err() {
                        self.last_reject = REJECT_UNDECODABLE_CODE;
                        return Err(DriverError::OutOfRange);
                    }
                    return Ok(event);
                }
                // An event of a type the consumer does not handle (e.g.
                // an asynchronous controller event interleaved with the
                // transfer/command completion). Surfaced, not absorbed,
                // with its raw type retained for the metal diagnostic.
                _ => {
                    self.last_reject = REJECT_UNEXPECTED_TYPE;
                    return Err(DriverError::DeviceFault);
                }
            }
        }
        self.last_reject = REJECT_BUDGET_TIMEOUT;
        Err(DriverError::DeviceFault)
    }

    /// Issue one command TRB and wait for its successful completion.
    fn command(&mut self, command: Trb) -> Result<Trb, DriverError> {
        self.reset_event_diagnostics();
        let outcome = self.command_ring.push(command)?;
        publish(
            &mut self.dma,
            self.layout.command_ring,
            self.command_ring.link_slot(),
            &outcome,
        )?;
        self.xhci.ring_doorbell(0, 0)?;
        // `await_event_for` records the raw completion code as it sees
        // the event, so `last_completion_code()` is meaningful even
        // when this validation rejects it below.
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

    /// Read one device-context block (the [`CTX_DWORDS`] dwords at
    /// `offset`) back out of DMA, for copying a controller-maintained
    /// output context into the input context before re-issuing a command
    /// over it (xHCI §4.6.6: a Configure Endpoint preserves the fields it
    /// does not touch, so the input copy must start from the live output
    /// context).
    fn read_ctx(&mut self, offset: usize) -> Result<[u32; CTX_DWORDS], DriverError> {
        let mut bytes = [0u8; CTX_DWORDS * 4];
        self.dma.read(offset, &mut bytes)?;
        let mut dwords = [0u32; CTX_DWORDS];
        for (index, dword) in dwords.iter_mut().enumerate() {
            *dword = u32::from_le_bytes([
                bytes[index * 4],
                bytes[index * 4 + 1],
                bytes[index * 4 + 2],
                bytes[index * 4 + 3],
            ]);
        }
        Ok(dwords)
    }

    /// Run one control transfer on the default endpoint: `setup`,
    /// an optional IN data stage of `data_in_len` bytes into the
    /// control data buffer, and the status stage. Returns the bytes
    /// the device actually delivered.
    fn control(&mut self, setup: [u8; 8], data_in_len: u32) -> Result<u32, DriverError> {
        if data_in_len as usize > CTRL_DATA_LEN {
            return Err(DriverError::LengthOutOfRange);
        }
        self.reset_event_diagnostics();
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
            self.ep0_ring_off,
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
                self.ep0_ring_off,
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
            self.ep0_ring_off,
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

    /// Run an *optional* control request (no data stage), tolerating a
    /// protocol STALL.
    ///
    /// A device that does not implement an optional class request (e.g.
    /// `SET_PROTOCOL`, mandatory only for boot-subclass devices) STALLs it;
    /// per USB 2.0 §8.5.3.4 the endpoint resumes on the next SETUP. This is
    /// the last EP0 transfer of enumeration, so a STALL is absorbed rather
    /// than aborting an otherwise-enumerable keyboard. Every other
    /// completion still fails closed; the raw code is preserved in
    /// [`Self::last_completion_code`].
    fn control_optional(&mut self, setup: [u8; 8]) -> Result<(), DriverError> {
        match self.control(setup, 0) {
            Ok(_) => Ok(()),
            Err(DriverError::DeviceFault)
                if self.last_completion == CompletionCode::StallError.as_u8() =>
            {
                Ok(())
            }
            Err(other) => Err(other),
        }
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

    /// Address the device in `slot` (§4.3.4): program the input control
    /// context (A0 | A1), the slot context from `base` (speed, root-hub
    /// port, and — for a downstream device — Route String and TT) and the
    /// EP0 context, point the DCBAA at the active output context, then
    /// issue Address Device. The EP0 context points at the active EP0 ring
    /// ([`Self::ep0_ring_off`]), so a downstream device addressed after
    /// [`Self::rebind_to_downstream_region`] gets its own ring.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the controller rejects the command.
    fn address_device(
        &mut self,
        base: SlotCtxBase,
        slot: u8,
        max_packet: u32,
    ) -> Result<(), DriverError> {
        self.write_input_ctx(0, &input_control_dwords(0b11))?;
        self.write_input_ctx(1, &slot_ctx_dwords(base, u32::from(DCI_CONTROL)))?;
        self.write_input_ctx(
            1 + usize::from(DCI_CONTROL),
            &ep_ctx_dwords(
                EP_TYPE_CONTROL,
                max_packet,
                0,
                self.phys_of(self.ep0_ring_off),
            ),
        )?;
        let output_ctx = self.phys_of(self.output_ctx_off);
        self.dma.write(
            self.layout.dcbaa + usize::from(slot) * 8,
            &output_ctx.to_le_bytes(),
        )?;
        self.stage = EnumStage::AddressDevice;
        self.command(Trb::new(
            TrbType::AddressDevice,
            self.phys_of(self.layout.input_ctx),
            0,
            trb::control_slot(slot),
        ))?;
        Ok(())
    }

    /// Bring the device on root-hub `port` to the configured state:
    /// port reset (when not yet enabled), Enable Slot, Address Device,
    /// `GET_DESCRIPTOR(device)`/`(configuration)`, and `SET_CONFIGURATION`.
    /// A **HID** interface additionally gets its interrupt-IN endpoint
    /// configured, a best-effort `SET_PROTOCOL(boot)`, and a primed
    /// interrupt-IN ring; a non-HID interface (e.g. a hub, class `0x09`)
    /// uses only its control endpoint.
    ///
    /// The interrupt-IN endpoint is armed and doorbelled **only** for a HID
    /// interface: arming a hub's status-change endpoint (which this engine
    /// never reads) would deliver asynchronous reports that interleave with
    /// the EP0 hub-class `GET_STATUS` transfers and wedge the control ring.
    /// `SET_PROTOCOL(boot)` is likewise HID-only, since a non-HID interface
    /// STALLs it and halts the control endpoint.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Busy`] if a device was already enumerated here.
    /// * [`DriverError::OutOfRange`] if `port` is out of range.
    /// * [`DriverError::BadMagic`] if the device descriptor is forged.
    /// * [`DriverError::DeviceFault`] for any controller/device failure.
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
            self.stage = EnumStage::PortReset;
            self.xhci.reset_port(port, self.budget)?
        };
        let max_packet = ep0_max_packet(status.speed())?;

        self.stage = EnumStage::EnableSlot;
        let event = self.command(Trb::new(TrbType::EnableSlot, 0, 0, 0))?;
        let slot = event.slot_id();
        if slot == 0 || slot > self.xhci.max_slots() {
            return Err(DriverError::DeviceFault);
        }
        self.slot = slot;

        self.root_port = port;
        let base = SlotCtxBase::root(status.speed(), port);
        self.address_device(base, slot, max_packet)?;
        self.finish_enumeration(slot, base)
    }

    /// Complete enumeration of the device already Enable-Slotted into
    /// `slot` and Address-Deviced with topology `base`: read its device
    /// and configuration descriptors and, for a HID interface, configure
    /// the interrupt-IN endpoint, `SET_CONFIGURATION`, best-effort
    /// `SET_PROTOCOL(boot)`, and prime the report ring.
    ///
    /// Shared by the root-hub ([`Self::enumerate_hid`]) and downstream
    /// ([`Self::enumerate_downstream_hid`]) paths so the post-Address
    /// sequence is written once; they differ only in the
    /// topology carried in `base`. The interrupt-IN endpoint is armed only
    /// for a HID interface (see [`Self::enumerate_hid`]).
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] if a descriptor is forged.
    /// * [`DriverError::DeviceFault`] for any controller/device failure.
    fn finish_enumeration(
        &mut self,
        slot: u8,
        base: SlotCtxBase,
    ) -> Result<DeviceDescriptor, DriverError> {
        self.stage = EnumStage::GetDeviceDescriptor;
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
        // class and number rather than assuming. The whole buffer is
        // requested; the device short-packets at the real length.
        let config_buf_len =
            u32::try_from(CTRL_DATA_LEN).map_err(|_| DriverError::LengthOutOfRange)?;
        let config_len_u16 =
            u16::try_from(CTRL_DATA_LEN).map_err(|_| DriverError::LengthOutOfRange)?;
        self.stage = EnumStage::GetConfigDescriptor;
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

        // Arm the interrupt-IN endpoint only for a HID interface; a hub
        // uses only its control endpoint (arming a hub's status-change
        // endpoint wedges its EP0 ring — see `enumerate_hid`).
        if interface.is_hid() {
            // Configure the interrupt-IN endpoint the descriptor reports
            // (DCI, max packet size, service interval — never assumed),
            // raising the slot's context entries to cover that DCI.
            let int_dci = interface.int_dci;
            let max_packet = u32::from(interface.int_max_packet);
            let interval = interrupt_interval(base.speed, interface.int_b_interval);
            self.write_input_ctx(0, &input_control_dwords(1 | (1u32 << u32::from(int_dci))))?;
            self.write_input_ctx(1, &slot_ctx_dwords(base, u32::from(int_dci)))?;
            self.write_input_ctx(
                1 + usize::from(int_dci),
                &ep_ctx_dwords(
                    EP_TYPE_INTERRUPT_IN,
                    max_packet,
                    interval,
                    self.phys_of(self.layout.int_ring),
                ),
            )?;
            self.int_dci = int_dci;
            self.stage = EnumStage::ConfigureEndpoint;
            self.command(Trb::new(
                TrbType::ConfigureEndpoint,
                self.phys_of(self.layout.input_ctx),
                0,
                trb::control_slot(slot),
            ))?;
        }

        self.stage = EnumStage::SetConfiguration;
        self.control(setup_set_configuration(interface.configuration_value), 0)?;

        if interface.is_hid() {
            // `SET_PROTOCOL(boot)`, the last EP0 transfer; a device that
            // does not implement it STALLs, which is tolerated.
            self.stage = EnumStage::SetProtocol;
            self.control_optional(setup_set_protocol_boot(interface.interface_number))?;

            self.stage = EnumStage::ArmReport;
            for _ in 0..PRIMED_REPORTS {
                self.arm_report()?;
            }
            self.xhci.ring_doorbell(slot, u32::from(self.int_dci))?;
            self.identity = Some(HidIdentity {
                vendor_id: descriptor.vendor_id,
                product_id: descriptor.product_id,
                interface_class: interface.class24,
            });
        }
        self.stage = EnumStage::Configured;
        Ok(descriptor)
    }

    /// Bring up the first root-hub port reporting a connected device.
    ///
    /// xHCI numbers root-hub ports from `1`. First asserts Port Power on
    /// every port (the [`Xhci::open`] reset cleared `PORTSC`, and a
    /// port-power-controlled controller reports a powered-off port as
    /// disconnected, xHCI 1.2 §4.19.1.1), then polls `1..=max_ports` —
    /// bounded by the budget — for the first connected port and enumerates
    /// it ([`Self::enumerate_hid`]). For the boot-keyboard case where the
    /// physical port is unknown, rather than guessing.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if no port connects before the budget is
    ///   spent (an empty root hub — fail closed, never a guessed port).
    /// * Any error of [`Self::enumerate_hid`], [`Xhci::set_port_power`], or
    ///   a faulting port-status read.
    pub fn enumerate_first_connected(&mut self) -> Result<DeviceDescriptor, DriverError> {
        let max_ports = self.xhci.max_ports();
        for port in 1..=max_ports {
            self.xhci.set_port_power(port)?;
        }
        // Poll for the first connected port, bounded by the budget (the
        // attach debounces over the powered-on settle window). An empty hub
        // spends the budget and fails closed.
        for _ in 0..self.budget {
            for port in 1..=max_ports {
                if self.xhci.port_status(port)?.connected() {
                    return self.enumerate_hid(port);
                }
            }
        }
        Err(DriverError::NotFound)
    }

    /// Bring up a HID boot keyboard reachable through the controller,
    /// transparently descending one tier through a USB hub.
    ///
    /// This is the arch-neutral bring-up orchestration a keyboard driver
    /// runs once after [`Self::start`]: it enumerates the first connected
    /// root-hub port ([`Self::enumerate_first_connected`]) and, if that
    /// device is itself a hub (the Raspberry Pi 4's onboard hub is — the
    /// keyboard hangs off a downstream port), powers the hub's downstream
    /// ports, finds the first connected one, resets it, and addresses the
    /// device behind it on a second xHCI slot
    /// ([`Self::enumerate_downstream_hid`]). On success [`Self::slot`] is
    /// the keyboard's slot and [`ReportSource::next_report`] drains its
    /// reports; the engine holds no logging dependency, so a driver wraps
    /// this with its own diagnostics.
    ///
    /// `delay` supplies the hardware-dictated settle windows (hub
    /// power-on-good and reset-recovery); the caller owns the clock.
    /// Only one tier of hub is descended — the boot-keyboard topology
    /// the Pi 4 presents — rather than a recursive bus walk; a keyboard
    /// nested two hubs deep is left unreached fail-closed rather than
    /// guessed at.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if no device connects on the root hub,
    ///   or the root device is a hub with no connected downstream port
    ///   (fail closed — never a guessed port).
    /// * [`DriverError::DeviceFault`] if a reset downstream port does not
    ///   report enabled (it never established a speed/TT, so addressing it
    ///   would be a guess).
    /// * Any error of [`Self::enumerate_first_connected`],
    ///   [`Self::hub_num_ports`], [`Self::power_hub_port`],
    ///   [`Self::hub_port_status`], [`Self::reset_hub_port`], or
    ///   [`Self::enumerate_downstream_hid`].
    pub fn enumerate_boot_keyboard(
        &mut self,
        delay: &dyn Delay,
    ) -> Result<DeviceDescriptor, DriverError> {
        let descriptor = self.enumerate_first_connected()?;
        if !descriptor.is_hub() {
            // The keyboard is attached directly to a root-hub port.
            return Ok(descriptor);
        }
        // The root device is a hub (the Pi 4's onboard hub); the keyboard
        // is one tier below it. Power every downstream port and let the
        // power-on-good window elapse before reading connect status.
        let num_ports = self.hub_num_ports()?;
        for port in 1..=num_ports {
            self.power_hub_port(port)?;
        }
        delay.delay_us(HUB_POWER_ON_GOOD_US);
        let mut connected_port = 0u8;
        for port in 1..=num_ports {
            if let Ok(status) = self.hub_port_status(port) {
                if hub_port_connected(status) {
                    connected_port = port;
                    break;
                }
            }
        }
        if connected_port == 0 {
            return Err(DriverError::NotFound);
        }
        // Reset the port so the hub enables it and establishes its speed
        // and transaction translator, then wait the reset-recovery window
        // before reading the port enabled.
        self.reset_hub_port(connected_port)?;
        delay.delay_us(HUB_RESET_RECOVERY_US);
        let status = self.hub_port_status(connected_port)?;
        if !hub_port_enabled(status) {
            return Err(DriverError::DeviceFault);
        }
        let speed = hub_port_speed(status);
        self.enumerate_downstream_hid(connected_port, speed)
    }

    /// Number of root-hub ports the controller reports
    /// (`HCSPARAMS1` `MaxPorts`).
    ///
    /// For a one-shot diagnostic that walks every root-hub port's
    /// `PORTSC` ([`Self::root_port_status_raw`]); the bring-up itself
    /// uses [`Self::enumerate_first_connected`].
    #[must_use]
    pub fn root_port_count(&self) -> u8 {
        self.xhci.max_ports()
    }

    /// Raw `PORTSC` dword of root-hub `port` (1-based), for a one-shot
    /// diagnostic capture of every port's connect/power/enable/speed state.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `port` is zero or above
    ///   [`Self::root_port_count`].
    /// * [`DriverError::DeviceFault`] if the register window rejects the read.
    pub fn root_port_status_raw(&mut self, port: u8) -> Result<u32, DriverError> {
        Ok(self.xhci.port_status(port)?.raw())
    }

    /// Read a configured hub's topology from its hub class descriptor (USB
    /// 2.0 §11.23.2.1): `bNbrPorts` and the TT Think Time in
    /// `wHubCharacteristics` bits 5:6. The caller must already have
    /// enumerated the device and confirmed it is a hub.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] for a non-hub or too-short reply.
    /// * [`DriverError::DeviceFault`] if the control transfer faults.
    fn read_hub_topology(&mut self) -> Result<(u8, u8), DriverError> {
        // `bNbrPorts` is byte 2 and `wHubCharacteristics` bytes 3:4, so a
        // short read of the leading bytes is enough.
        const HUB_DESC_REQUEST: usize = 8;
        let want = u16::try_from(HUB_DESC_REQUEST).map_err(|_| DriverError::LengthOutOfRange)?;
        let transferred = self.control(setup_get_hub_descriptor(want), u32::from(want))?;
        if (transferred as usize) < 5 {
            return Err(DriverError::BadMagic);
        }
        let mut desc = [0u8; HUB_DESC_REQUEST];
        self.dma.read(self.layout.ctrl_data, &mut desc)?;
        if desc[1] != DESC_TYPE_HUB {
            return Err(DriverError::BadMagic);
        }
        let tt_think_time = (u16::from_le_bytes([desc[3], desc[4]]) >> 5) & 0b11;
        Ok((desc[2], tt_think_time as u8))
    }

    /// Read a configured hub's `bNbrPorts` (downstream port count) from its
    /// hub class descriptor (USB 2.0 §11.23.2.1).
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] for a non-hub or too-short reply.
    /// * [`DriverError::DeviceFault`] if the control transfer faults.
    pub fn hub_num_ports(&mut self) -> Result<u8, DriverError> {
        Ok(self.read_hub_topology()?.0)
    }

    /// Set the **Hub** bit in the active slot's context (xHCI §6.2.2) so the
    /// controller routes and splits the transactions of devices addressed
    /// downstream of it — otherwise a device behind the hub is addressed
    /// but never delivers a report. Issues an `A0`-only Configure Endpoint
    /// copying the live output slot context and setting the Hub bit, Number
    /// of Ports, and TT Think Time from the hub descriptor (single-TT).
    /// Must run while the hub is the active slot, before
    /// [`Self::rebind_to_downstream_region`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] if the hub descriptor is forged.
    /// * [`DriverError::DeviceFault`] if the controller rejects the command.
    fn configure_hub_slot(&mut self) -> Result<(), DriverError> {
        let (num_ports, tt_think_time) = self.read_hub_topology()?;
        let mut slot = self.read_ctx(self.output_ctx_off)?;
        slot[0] = (slot[0] | SLOT_CTX_HUB) & !SLOT_CTX_MTT;
        slot[1] = (slot[1] & !(0xFFu32 << SLOT_CTX_NUM_PORTS_SHIFT))
            | (u32::from(num_ports) << SLOT_CTX_NUM_PORTS_SHIFT);
        slot[2] = (slot[2] & !SLOT_CTX_TTT_MASK) | (u32::from(tt_think_time) << SLOT_CTX_TTT_SHIFT);
        self.write_input_ctx(0, &input_control_dwords(1))?;
        self.write_input_ctx(1, &slot)?;
        self.stage = EnumStage::ConfigureEndpoint;
        self.command(Trb::new(
            TrbType::ConfigureEndpoint,
            self.phys_of(self.layout.input_ctx),
            0,
            trb::control_slot(self.slot),
        ))?;
        Ok(())
    }

    /// Assert `PORT_POWER` on downstream hub `port` (1-based) via a class
    /// `SET_FEATURE` (USB 2.0 §11.24.2.13). A port-power-controlled hub
    /// reports a port disconnected until this is set; the caller waits the
    /// power-on-good time before reading [`Self::hub_port_status`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the control transfer faults.
    pub fn power_hub_port(&mut self, port: u8) -> Result<(), DriverError> {
        self.control(setup_set_port_feature(PORT_FEATURE_POWER, port), 0)
            .map(|_| ())
    }

    /// Read downstream hub `port`'s 16-bit `wPortStatus` via a class
    /// `GET_STATUS` (USB 2.0 §11.24.2.7).
    ///
    /// Decode it with [`hub_port_connected`] and [`hub_port_speed`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the control transfer faults or
    ///   the device returns fewer than the two `wPortStatus` bytes
    ///   (fail closed).
    pub fn hub_port_status(&mut self, port: u8) -> Result<u16, DriverError> {
        let transferred = self.control(setup_get_port_status(port), 4)?;
        if transferred < 2 {
            return Err(DriverError::DeviceFault);
        }
        let mut buf = [0u8; 4];
        self.dma.read(self.layout.ctrl_data, &mut buf)?;
        Ok(u16::from_le_bytes([buf[0], buf[1]]))
    }

    /// Reset downstream hub `port` (1-based) via a class
    /// `SET_FEATURE(PORT_RESET)` (USB 2.0 §11.24.2.13).
    ///
    /// A downstream device is enabled — and its speed (and, for a
    /// full/low-speed device, its transaction translator) established —
    /// only once its hub port has been reset. The caller waits the
    /// reset-recovery time before reading [`Self::hub_port_status`] to
    /// confirm [`hub_port_enabled`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the control transfer faults
    ///   (fail closed).
    pub fn reset_hub_port(&mut self, port: u8) -> Result<(), DriverError> {
        self.control(setup_set_port_feature(PORT_FEATURE_RESET, port), 0)
            .map(|_| ())
    }

    /// Rebind the active default-control endpoint to the second slot's
    /// region ([`Layout::ep0_ring2`] / [`Layout::output_ctx2`]) so the
    /// next [`Self::address_device`] addresses a *downstream* device on a
    /// fresh ring and output context, leaving the hub's first-region
    /// ring and output context live in the DCBAA.
    ///
    /// Initialises the second EP0 ring's Link TRB exactly as
    /// [`Self::start`] does for the first.
    fn rebind_to_downstream_region(&mut self) -> Result<(), DriverError> {
        let base = self.phys_of(self.layout.ep0_ring2);
        let (ring, link) = ProducerRing::new(RING_TRBS, base)?;
        self.dma.write(
            self.layout.ep0_ring2 + ring.link_slot() * trb::TRB_LEN,
            &link.to_bytes(),
        )?;
        self.ep0_ring = ring;
        self.ep0_ring_off = self.layout.ep0_ring2;
        self.output_ctx_off = self.layout.output_ctx2;
        Ok(())
    }

    /// Address and configure the HID device on downstream hub `down_port`
    /// (1-based) — reached through the hub already enumerated on the
    /// active slot — at protocol speed `speed` ([`hub_port_speed`]).
    ///
    /// This is the second xHCI slot: the hub stays addressed on its slot
    /// (its output context and EP0 ring untouched), and the downstream
    /// device gets a fresh slot whose context carries the **Route
    /// String** (the hub's downstream port) and — for a full/low-
    /// speed device behind the high-speed hub — the **TT** Hub Slot ID
    /// and Port Number (§6.2.2), so the controller splits its
    /// transactions through the hub's transaction translator. The device
    /// gets its own EP0 ring and output context (`rebind_to_downstream_region`);
    /// enumeration then proceeds exactly as for a root-port device (the
    /// `finish_enumeration` sequence shared with [`Self::enumerate_hid`]).
    ///
    /// The caller (which owns the wall-clock delays) must have already
    /// powered the port, reset it ([`Self::reset_hub_port`]), and
    /// confirmed it [`hub_port_enabled`] with the `speed` read from
    /// [`Self::hub_port_status`].
    ///
    /// On success [`Self::slot`] becomes the downstream device's slot and
    /// [`ReportSource::next_report`] drains its reports.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if no hub is addressed on the
    ///   active slot, the controller assigns no fresh slot, or any
    ///   command/transfer faults (fail closed).
    /// * [`DriverError::BadMagic`] if a descriptor is forged.
    pub fn enumerate_downstream_hid(
        &mut self,
        down_port: u8,
        speed: u8,
    ) -> Result<DeviceDescriptor, DriverError> {
        // A hub must be addressed on the active slot for the route
        // string's root-port and TT-hub-slot to be meaningful.
        if self.slot == 0 {
            return Err(DriverError::DeviceFault);
        }
        let hub_slot = self.slot;
        let root_port = self.root_port;
        let max_packet = ep0_max_packet(speed)?;

        // Tell the controller the active slot is a hub (Hub bit + ports +
        // TT Think Time) before addressing anything behind it; otherwise
        // it never schedules the downstream device's split transactions
        // and the keyboard is addressed but never reports (xHCI §6.2.2).
        // Issued while the hub is still the active slot, before the rebind
        // re-points EP0 at the downstream device.
        self.configure_hub_slot()?;

        self.rebind_to_downstream_region()?;
        self.stage = EnumStage::EnableSlot;
        let event = self.command(Trb::new(TrbType::EnableSlot, 0, 0, 0))?;
        let slot = event.slot_id();
        if slot == 0 || slot > self.xhci.max_slots() || slot == hub_slot {
            return Err(DriverError::DeviceFault);
        }
        self.slot = slot;

        // One tier below the root hub: the device sits at the hub's
        // downstream port number in the lowest Route String nibble.
        let route_string = u32::from(down_port) & 0xF;
        // A full/low-speed device behind this high-speed hub routes
        // through the hub's transaction translator; a high-speed device
        // needs none (§6.2.2).
        let (tt_hub_slot, tt_port) = if speed_needs_tt(speed) {
            (hub_slot, down_port)
        } else {
            (0, 0)
        };
        let base = SlotCtxBase {
            speed,
            root_port,
            route_string,
            tt_hub_slot,
            tt_port,
        };
        self.address_device(base, slot, max_packet)?;
        self.finish_enumeration(slot, base)
    }

    /// The enumeration step [`Self::enumerate_hid`] last entered.
    ///
    /// After a [`Self::enumerate_first_connected`] failure this pins
    /// which xHCI operation a [`DriverError::DeviceFault`] came from —
    /// [`EnumStage::Scan`] means no connected port was ever entered (an
    /// empty hub / [`DriverError::NotFound`]); any later variant names
    /// the faulting step.
    #[must_use]
    pub const fn enum_stage(&self) -> EnumStage {
        self.stage
    }

    /// Raw completion code of the most recent event TRB the last
    /// command/control transfer observed (`0` = none seen since that
    /// transfer began — a timeout), pairing with [`Self::enum_stage`]
    /// to distinguish a stuck controller from a device that answered
    /// with an error code.
    #[must_use]
    pub const fn last_completion_code(&self) -> u8 {
        self.last_completion
    }

    /// Raw TRB-type of the most recent event the last command/control
    /// transfer's event wait observed (`0` = none seen).
    ///
    /// Paired with [`Self::last_reject_reason`] this names *what* an
    /// unexpected-event reject saw — e.g. an asynchronous controller
    /// event interleaved with the awaited completion — which the
    /// completion code alone cannot.
    #[must_use]
    pub const fn last_event_type(&self) -> u8 {
        self.last_event_type
    }

    /// Why the last command/control transfer's event wait
    /// failed: `0` none (it succeeded, or none has run), `1` an event of
    /// an unhandled TRB-type (see [`Self::last_event_type`]), `2` a
    /// completion for a TRB the transfer did not enqueue, `3` an
    /// undecodable completion code (see [`Self::last_completion_code`]),
    /// `4` the poll budget elapsed with no event (a genuine timeout).
    ///
    /// This distinguishes a fast reject (a real but unexpected event)
    /// from a true timeout, which `completion_hex=0` alone conflates.
    #[must_use]
    pub const fn last_reject_reason(&self) -> u8 {
        self.last_reject
    }

    /// Read the controller's `USBCMD` for a one-shot bring-up diagnostic
    /// (delegates to [`Xhci::read_usbcmd`]), or `None` if the read faults.
    pub fn read_usbcmd(&mut self) -> Option<u32> {
        self.xhci.read_usbcmd()
    }

    /// Read the controller's `USBSTS` for a one-shot bring-up diagnostic
    /// (delegates to [`Xhci::read_usbsts`]), or `None` if the read faults.
    pub fn read_usbsts(&mut self) -> Option<u32> {
        self.xhci.read_usbsts()
    }

    /// Raw `PORTSC` of root-hub `port` (1-based) for a bring-up diagnostic,
    /// or `None` if the port is out of range or the read faults. A capture
    /// of the connect/power/enable/speed bits when enumeration stalls on a
    /// root port.
    pub fn port_status_raw(&mut self, port: u8) -> Option<u32> {
        self.xhci.port_status(port).ok().map(crate::PortStatus::raw)
    }

    /// Describe the enumerated HID device as a discovered child
    /// [`HwNode`] parented at `parent_id` and assigned `node_id`.
    ///
    /// The node carries one [`HwMatchKey::usb`] of the device's
    /// `vid:pid` and the 24-bit class of the interface this driver
    /// brought up — both read from the device during
    /// [`Self::enumerate_hid`], never assumed — so `devmgr` resolves an
    /// HID driver's signed bind table against it. Its [`HwDeviceClass`] is [`HwDeviceClass::Input`], the
    /// HID-class match key mirroring the PCI child node
    /// [`PciBus::describe_function`](rustos_abi::driver::pci::PciBus::describe_function)
    /// emits for the controller above it.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if no device has been enumerated yet
    ///   (the identity is captured only on a successful
    ///   [`Self::enumerate_hid`]) — fail closed, never a fabricated node.
    /// * [`DriverError::DeviceFault`] if the match key cannot be pushed.
    ///
    /// # Capabilities
    ///
    /// None — describing a node mints no resources (:
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

impl<H: XhciHost, M: DmaRegion> UsbDevice<H, M> {
    /// Decode one completed interrupt-IN [`TrbType::TransferEvent`] (already
    /// confirmed to target this device's slot and interrupt endpoint) into a
    /// report length, copying the report bytes into `buf`.
    ///
    /// This performs only the *validation and copy* of one transfer; it does
    /// **not** touch the transfer ring. Re-arming the endpoint is the caller's
    /// (`next_report`) unconditional responsibility, so that a transfer whose
    /// completion code or buffer mapping this method rejects still leaves the
    /// endpoint re-armed for the next report (a single odd transfer must never
    /// silence the keyboard).
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] for an unexpected completion code, a
    /// completed-TRB address outside the interrupt ring, a misaligned or
    /// out-of-range ring slot, or a residual larger than the report.
    fn decode_transfer_report(&mut self, event: Trb, buf: &mut [u8]) -> Result<usize, DriverError> {
        match event.completion_code() {
            Ok(CompletionCode::Success | CompletionCode::ShortPacket) => {}
            _ => return Err(DriverError::DeviceFault),
        }
        // Map the completed TRB back to its slot's report buffer,
        // validating every step of the controller's claim.
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
        Ok(len)
    }

    /// Retire the just-completed interrupt-IN transfer and prime a fresh one,
    /// ringing the endpoint doorbell so the controller delivers the next
    /// report.
    ///
    /// Called by [`ReportSource::next_report`] for **every** completed
    /// transfer event addressed to this endpoint — including one whose report
    /// was rejected by [`Self::decode_transfer_report`] — so the interrupt
    /// endpoint is never left un-armed (which would permanently stall input:
    /// a busy-polling driver would keep reading an empty event ring forever
    /// while the keyboard appears dead).
    ///
    /// # Errors
    ///
    /// Propagates a transfer-ring or doorbell failure ([`DriverError::Busy`]
    /// if the ring is unexpectedly full, or the register-window error from the
    /// doorbell write).
    fn rearm_interrupt_endpoint(&mut self) -> Result<(), DriverError> {
        self.int_ring.retire_one()?;
        self.arm_report()?;
        self.xhci.ring_doorbell(self.slot, u32::from(self.int_dci))
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
                // No event pending.
                return Ok(None);
            };
            match event.trb_type() {
                Ok(TrbType::PortStatusChange) => continue,
                Ok(TrbType::TransferEvent) => {}
                _ => return Err(DriverError::DeviceFault),
            }
            if event.slot_id() != self.slot || event.endpoint_id() != self.int_dci {
                // Not this endpoint's transfer — surface the controller fault
                // without disturbing our own transfer ring.
                return Err(DriverError::DeviceFault);
            }
            // Decode this completed transfer first, then re-arm the endpoint
            // **unconditionally**: an unexpected completion code or a
            // malformed buffer mapping is surfaced as a per-report error, but
            // the endpoint is always re-primed so a single odd report can
            // never leave it silent and wedge the keyboard.
            let decoded = self.decode_transfer_report(event, buf);
            self.rearm_interrupt_endpoint()?;
            return decoded.map(Some);
        }
        Ok(None)
    }
}

impl<H: XhciHost, M: DmaRegion> crate::transport::UrbEngine for UsbDevice<H, M> {
    fn control_in(&mut self, setup: [u8; 8], data: &mut [u8]) -> Result<usize, DriverError> {
        // The engine's control transfer lands the IN data in the device's
        // control-data DMA buffer; copy out only the bytes the device
        // delivered, never past the caller's shared buffer.
        let requested = u32::try_from(data.len()).map_err(|_| DriverError::LengthOutOfRange)?;
        let transferred = self.control(setup, requested)?;
        let transferred = usize::try_from(transferred).map_err(|_| DriverError::DeviceFault)?;
        let copied = transferred.min(data.len());
        self.dma.read(self.layout.ctrl_data, &mut data[..copied])?;
        Ok(copied)
    }

    fn interrupt_in(&mut self, data: &mut [u8]) -> Result<Option<usize>, DriverError> {
        self.next_report(data)
    }
}
