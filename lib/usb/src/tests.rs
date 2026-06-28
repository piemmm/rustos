//! Unit tests for the xHCI protocol layers against a register-level
//! mock controller plus an in-memory ring/DMA model (mirrors the
//! `emmc2` `MockSdhci` seam).

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use super::device::{
    hub_port_connected, hub_port_enabled, hub_port_speed, BringUp, DeviceDescriptor, DmaRegion,
    EnumStage, HubEvent, InterfaceInfo, UsbDevice, EVENT_RING_SEGMENT_MIN_TRBS, REPORT_LEN,
    RING_TRBS,
};
use super::ring::{EventRingCursor, ProducerRing};
use super::trb::{CompletionCode, Trb, TrbType, CONTROL_CYCLE, TRB_LEN};
use super::*;
use rustos_abi::driver::input::{Input, ReportSource};
use rustos_abi::Delay;
use rustos_hid::BootKeyboard;

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
/// 64-byte contexts, plus the hub status-change ring and report buffer,
/// needs ~5.4 KiB).
const MOCK_DMA_LEN: usize = 0x4000;
/// The mock's 64-byte contexts (its `HCCPARAMS1` sets CSZ).
const MOCK_CTX_SIZE: usize = 64;

#[test]
fn event_ring_segment_meets_xhci_minimum() {
    let ring_trbs = core::hint::black_box(RING_TRBS);
    let event_min = core::hint::black_box(EVENT_RING_SEGMENT_MIN_TRBS);
    assert!(ring_trbs >= event_min);
    assert_eq!(ring_trbs, 16);
}

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

/// File-scope recorder for the [`DmaSlab`] coherency hook (a bare `fn`
/// pointer, so the observed call count and length are published through
/// atomics). Used by a single test so no cross-test race is possible
/// (no flaky tests).
mod slab_coherency_test_state {
    use core::sync::atomic::{AtomicUsize, Ordering};
    pub(super) static CALLS: AtomicUsize = AtomicUsize::new(0);
    pub(super) static LAST_LEN: AtomicUsize = AtomicUsize::new(0);

    /// A `rustos_abi::driver::dma::SlabCoherencyFn`.
    pub(super) fn record(_base: *const u8, len: usize) {
        CALLS.fetch_add(1, Ordering::SeqCst);
        LAST_LEN.store(len, Ordering::SeqCst);
    }
}

#[test]
fn dma_slab_region_brackets_writes_and_reads_with_cache_maintenance() {
    use core::ptr::NonNull;
    use core::sync::atomic::Ordering;
    use rustos_abi::driver::dma::{DmaSlab, PoolId};
    use slab_coherency_test_state as rec;

    // A leaked 64-byte buffer behind a `DmaSlab` carrying the recording
    // coherency hook — the metal shape where the BCM2711 PCIe master does
    // not snoop the CPU caches, so the `DmaRegion` impl must bracket every
    // ring publish / event consume with cache maintenance.
    let storage = alloc::vec![0u8; 64].into_boxed_slice();
    let phys = storage.as_ptr() as u64;
    let leaked: &'static mut [u8] = alloc::boxed::Box::leak(storage);
    let ptr = NonNull::new(leaked.as_mut_ptr()).expect("box leak is non-null");
    // SAFETY: the buffer is leaked (`'static`), exactly 64 bytes, and
    // nothing else references it.
    let mut slab =
        unsafe { DmaSlab::from_leaked(phys, ptr, 64, PoolId::MOCK, 0) }.with_coherency(rec::record);

    // A write cleans the published range to memory *after* the CPU copy,
    // so a non-coherent master reads fresh bytes once the doorbell rings.
    DmaRegion::write(&mut slab, 8, &[0xAB; 4]).expect("write");
    assert_eq!(rec::CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(rec::LAST_LEN.load(Ordering::SeqCst), 4);

    // A read invalidates the CPU's view of the range *before* the copy,
    // so a master's freshly written bytes are read from memory.
    let mut buf = [0u8; 2];
    DmaRegion::read(&mut slab, 16, &mut buf).expect("read");
    assert_eq!(rec::CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(rec::LAST_LEN.load(Ordering::SeqCst), 2);
}

/// The 18-byte device descriptor fixture the model answers
/// `GET_DESCRIPTOR(device)` with (a generic boot keyboard).
const MOCK_DESCRIPTOR: [u8; 18] = [
    18, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x6D, 0x04, 0x77, 0xC0, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x01,
];

/// The configuration descriptor fixture the model answers
/// `GET_DESCRIPTOR(configuration)` with: a 9-byte configuration header
/// (`bConfigurationValue` = 1) followed by one 9-byte interface
/// descriptor of the HID boot-keyboard class (`0x03_01_01`,
/// `bInterfaceNumber` = 0).
const MOCK_CONFIG_DESCRIPTOR: [u8; 25] = [
    // Configuration: bLength=9, type=2, wTotalLength=25, 1 interface,
    // bConfigurationValue=1, iConfiguration=0, bmAttributes=0xA0,
    // bMaxPower=50.
    0x09, 0x02, 0x19, 0x00, 0x01, 0x01, 0x00, 0xA0, 0x32, //
    // Interface: bLength=9, type=4, bInterfaceNumber=0, alt=0,
    // 1 endpoint, class=0x03 (HID), sub=0x01 (boot), protocol=0x01
    // (keyboard), iInterface=0.
    0x09, 0x04, 0x00, 0x00, 0x01, 0x03, 0x01, 0x01, 0x00, //
    // Endpoint: bLength=7, type=5, bEndpointAddress=0x81 (EP1 IN ->
    // DCI 3), bmAttributes=0x03 (interrupt), wMaxPacketSize=8,
    // bInterval=10 (frames, full-speed boot keyboard).
    0x07, 0x05, 0x81, 0x03, 0x08, 0x00, 0x0A,
];

/// As [`MOCK_CONFIG_DESCRIPTOR`], but the boot keyboard's interrupt-IN
/// endpoint is **endpoint 2** (`bEndpointAddress = 0x82` -> DCI 5), not
/// endpoint 1. The driver must read the endpoint descriptor and
/// configure / doorbell / drain DCI 5; the metal no-report bug was
/// hard-coding DCI 3.
const MOCK_CONFIG_DESCRIPTOR_EP2: [u8; 25] = [
    0x09, 0x02, 0x19, 0x00, 0x01, 0x01, 0x00, 0xA0, 0x32, //
    0x09, 0x04, 0x00, 0x00, 0x01, 0x03, 0x01, 0x01, 0x00, //
    // Endpoint: bEndpointAddress=0x82 (EP2 IN -> DCI 5).
    0x07, 0x05, 0x82, 0x03, 0x08, 0x00, 0x0A,
];

/// Device descriptor fixture for a USB **hub** (`bDeviceClass = 0x09`),
/// `idVendor:idProduct = 2109:3431` — the Pi 4B's onboard VIA Labs hub.
const MOCK_HUB_DESCRIPTOR: [u8; 18] = [
    18, 0x01, 0x00, 0x02, 0x09, 0x00, 0x00, 0x40, 0x09, 0x21, 0x31, 0x34, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x01,
];

/// Configuration descriptor fixture for the hub: one interface of the
/// hub class (`0x09_00_00`) with one interrupt-IN status-change endpoint
/// (USB 2.0 §11.12.3), so the engine arms the hub-hotplug watch.
const MOCK_HUB_CONFIG_DESCRIPTOR: [u8; 25] = [
    // Configuration: wTotalLength=25, 1 interface.
    0x09, 0x02, 0x19, 0x00, 0x01, 0x01, 0x00, 0xA0, 0x32, //
    // Interface: class=0x09 (hub), sub=0x00, protocol=0x00, 1 endpoint.
    0x09, 0x04, 0x00, 0x00, 0x01, 0x09, 0x00, 0x00, 0x00, //
    // Endpoint: bEndpointAddress=0x82 (EP2 IN -> DCI 5, distinct from a
    // downstream keyboard's DCI 3), interrupt, wMaxPacketSize=1 (the
    // port-change bitmap byte), bInterval=12.
    0x07, 0x05, 0x82, 0x03, 0x01, 0x00, 0x0C,
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
    /// When set, `USBSTS` reports a latched Host System Error until a
    /// host-controller reset clears it.
    hse_latched: bool,
    /// When set, `USBSTS` reports a latched Event Interrupt until a
    /// write-1-to-clear status write clears it.
    eint_latched: bool,
    /// When set, `USBSTS` reports a latched Port Change Detect until a
    /// write-1-to-clear status write clears it.
    pcd_latched: bool,
    /// When set, a status write is only made visible by the next
    /// `USBSTS` read, modelling a posted bridge write that must be
    /// flushed before the reset command.
    status_write_needs_read_flush: bool,
    pending_status_clear: u32,
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
    /// Interrupter 0 management register (`IMAN`): IE/IP bits.
    iman: u32,
    /// Interrupter 0 moderation register (`IMOD`).
    imod: u32,
    // Device-model ring consumer / event producer state.
    cmd_index: usize,
    cmd_cycle: bool,
    ep0_base: u64,
    ep0_index: usize,
    ep0_cycle: bool,
    /// The slot whose EP0 ring is currently the live `ep0_base`/`ep0_index`/
    /// `ep0_cycle`. The engine keeps a hub and a downstream device addressed
    /// at once and switches the active control context between them; a
    /// control doorbell for a different slot saves the live ring state and
    /// loads that slot's, mirroring the DCBAA-indexed hardware.
    ep0_slot: u8,
    /// Saved per-slot EP0 ring `(base, index, cycle)`, indexed by slot id.
    ep0_saved: [(u64, usize, bool); 33],
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
    /// When set, class requests (`SET_PROTOCOL`) answer STALL — the
    /// optional-request case `control_optional` tolerates.
    stall_class_requests: bool,
    /// When set, class requests (`SET_PROTOCOL`) answer a non-STALL
    /// transaction error — a genuine fault `control_optional` must
    /// still surface.
    fault_class_requests: bool,
    /// When set, report completions forge a residual above the TRB
    /// length (a hostile controller claim).
    forge_report_residual: bool,
    /// When set, the **next** interrupt report posts this completion code
    /// (instead of Success/ShortPacket) and clears the knob — modelling a
    /// single odd transfer event the driver rejects per-report. The
    /// endpoint must still be re-armed so the following report is
    /// delivered (a single rejected report must never silence the
    /// keyboard).
    fault_one_report_completion: Option<CompletionCode>,
    /// When set, a `DisableSlot` command posts **no** completion event,
    /// modelling the metal hot-removal where the gone device's hub never
    /// lets the controller acknowledge the Disable Slot in time. The
    /// best-effort teardown must still free the slot locally so a re-plug
    /// re-enumerates.
    suppress_disable_completion: bool,
    /// A root-hub port (0-based) whose device only reports Current
    /// Connect Status once software writes Port Power — modelling a
    /// port-power-controlled controller (the VL805, `HCCPARAMS1`
    /// PPC = 1), where an unpowered port reads disconnected.
    latent_device_port: Option<usize>,
    /// `HCSPARAMS2` value the mock reports (the split Max Scratchpad
    /// Buffers fields). `0` (default) needs no scratchpad; a non-zero
    /// count models the VL805, which executes **no** command until
    /// software points `DCBAA[0]` at a programmed scratchpad array
    /// (xHCI §4.20).
    hcsparams2: u32,
    /// `PAGESIZE` value the mock reports (`1` → 4 KiB scratchpad pages).
    pagesize: u32,
    /// When non-zero, the attached device is a USB **hub** reporting
    /// this many downstream ports; its device/config descriptors switch
    /// to the hub fixtures (class `0x09`), mirroring the Pi 4B's onboard
    /// `2109:3431` VIA Labs hub.
    hub_ports: u8,
    /// The 1-based downstream hub port a device is attached to (`0` =
    /// none), with that device's `wPortStatus` value.
    hub_downstream_port: u8,
    hub_downstream_status: u16,
    /// Bitmask of downstream hub ports software has powered (bit `n-1`
    /// for port `n`); a downstream port reports a connected device only
    /// once powered, modelling a port-power-controlled hub.
    hub_powered: u32,
    /// When set, the class `GET_DESCRIPTOR(hub)` reply carries a wrong
    /// `bDescriptorType` — a forged/corrupt descriptor the driver must
    /// reject fail-closed.
    forge_hub_descriptor: bool,
    /// Whether the default control endpoint is halted. A control
    /// transfer that STALLs halts EP0 in xHCI (§4.8.3 / §4.10.2.4): the
    /// controller runs no further TRBs on it until software resets the
    /// endpoint, so a subsequent control transfer faults. This models
    /// that, catching code that reuses EP0 after a tolerated STALL.
    ep0_halted: bool,
    /// When set, every downstream-port class `GET_STATUS` (USB 2.0
    /// §11.24.2.7) STALLs — modelling the metal failure where the
    /// hub-descriptor read succeeds but each per-port status read
    /// faults, so the bring-up diagnostic must surface the completion
    /// code.
    fault_hub_port_status: bool,
    /// When non-zero, every downstream-port class `GET_STATUS` posts a
    /// transfer event carrying this *raw* completion-code byte — used to
    /// model a controller-specific/reserved code the driver does not
    /// decode (the metal `completion_hex=0` was a code the diagnostic
    /// failed to record, not a true timeout).
    fault_hub_port_status_raw: u8,
    /// When non-zero, every downstream-port class `GET_STATUS` posts an
    /// event carrying this *raw TRB-type* (rather than a Transfer
    /// Event) — modelling an unexpected asynchronous controller event
    /// reaching the wait, which `await_event_for` rejects fast without
    /// recording a completion code (the metal `completion_hex=0` +
    /// fast-failure signature).
    fault_hub_port_status_evtype: u8,
    /// Bitmask of downstream hub ports software has reset (bit `n-1` for
    /// port `n`) via a class `SET_FEATURE(PORT_RESET)`; a reset port
    /// reports `PORT_STATUS_ENABLE` in its `wPortStatus`, the gate a
    /// downstream device must pass before it is addressed.
    hub_reset: u32,
    /// Set once an Address Device with a non-zero Route String has been
    /// processed: the active addressed device is now the **downstream**
    /// HID device (the keyboard behind the hub), so descriptor reads
    /// answer with the HID fixtures and the HID class requests succeed.
    downstream_active: bool,
    /// The downstream hub port the addressed device's Route String named,
    /// captured for the test to assert against.
    downstream_route_port: u8,
    /// Set once a Configure Endpoint that names only the slot context
    /// (Add flag `A0`) with the **Hub** bit set is processed: the parent
    /// hub has been marked a hub in its slot context, so the controller
    /// will schedule the split transactions a downstream device needs.
    /// Real hardware delivers no downstream interrupt transfer until
    /// this is done — the metal bug where the keyboard was addressed but
    /// never typed — so the mock gates [`Self::process_int_ring`] on it.
    hub_marked_as_hub: bool,
    /// The **Number of Ports** the hub-marking Configure Endpoint carried
    /// in the slot context (§6.2.2 dword 1), captured for assertions.
    hub_ctx_num_ports: u8,
    /// The **TT Think Time** the hub-marking Configure Endpoint carried
    /// in the slot context (§6.2.2 dword 2), captured for assertions.
    hub_ctx_tt_think_time: u8,
    /// The **Max ESIT Payload** the interrupt-IN Configure Endpoint
    /// carried in the endpoint context (§6.2.3.8 dword 4 bits 16:31).
    /// The xHCI periodic scheduler reserves no bandwidth for a periodic
    /// endpoint whose Max ESIT Payload is zero (§4.14.2), so real
    /// hardware delivers no interrupt transfer — the metal bug where the
    /// addressed keyboard never typed. The mock gates
    /// [`Self::process_int_ring`] on it being non-zero.
    int_max_esit: u32,
    /// The configuration-descriptor fixture answered for the keyboard
    /// (the non-hub device). A test can point this at a fixture whose
    /// interrupt endpoint is not endpoint 1 to prove the driver reads
    /// the endpoint's real DCI rather than assuming it.
    keyboard_config: &'static [u8],
    /// Device Context Index the interrupt-IN Configure Endpoint named,
    /// derived from its Add Context flags (§6.2.3) rather than assumed.
    /// The mock posts interrupt Transfer Events with it, so a keyboard
    /// whose interrupt endpoint is not endpoint 1 is serviced honestly
    /// (the metal no-report bug was the driver hard-coding DCI 3).
    int_dci: u8,
    /// The slot marked as a hub (the Configure Endpoint that raised the Hub
    /// bit), so a later endpoint-add on that slot is recognised as the hub's
    /// status-change endpoint rather than the downstream device's interrupt
    /// endpoint. `0` until a hub is marked.
    hub_slot_id: u8,
    /// The hub status-change endpoint's transfer-ring base / DCI / consumer
    /// state, set by the Configure Endpoint that adds it to the hub slot. The
    /// test posts a port-change report with [`Self::post_hub_status_change`].
    hub_int_base: u64,
    hub_int_dci: u8,
    hub_int_index: usize,
    hub_int_cycle: bool,
    /// `wPortChange` (USB 2.0 §11.24.2.7.2) the downstream-port `GET_STATUS`
    /// reports — the latched port changes (e.g. Connect Status Change). `0`
    /// = no change latched.
    hub_downstream_change: u16,
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
            hse_latched: false,
            eint_latched: false,
            pcd_latched: false,
            status_write_needs_read_flush: false,
            pending_status_clear: 0,
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
            iman: 0,
            imod: 0,
            cmd_index: 0,
            cmd_cycle: true,
            ep0_base: 0,
            ep0_index: 0,
            ep0_cycle: true,
            ep0_slot: 0,
            ep0_saved: [(0, 0, true); 33],
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
            fault_class_requests: false,
            forge_report_residual: false,
            fault_one_report_completion: None,
            suppress_disable_completion: false,
            latent_device_port: None,
            hcsparams2: 0,
            pagesize: 0,
            hub_ports: 0,
            hub_downstream_port: 0,
            hub_downstream_status: 0,
            hub_powered: 0,
            forge_hub_descriptor: false,
            ep0_halted: false,
            fault_hub_port_status: false,
            fault_hub_port_status_raw: 0,
            fault_hub_port_status_evtype: 0,
            hub_reset: 0,
            downstream_active: false,
            downstream_route_port: 0,
            hub_marked_as_hub: false,
            hub_ctx_num_ports: 0,
            hub_ctx_tt_think_time: 0,
            int_max_esit: 0,
            keyboard_config: &MOCK_CONFIG_DESCRIPTOR,
            int_dci: 3,
            hub_slot_id: 0,
            hub_int_base: 0,
            hub_int_dci: 0,
            hub_int_index: 0,
            hub_int_cycle: true,
            hub_downstream_change: 0,
        }
    }

    /// A mock with the device model attached as a USB **hub** on
    /// root-hub port 1 (a high-speed device, enabled), reporting `ports`
    /// downstream ports with a high-speed device on downstream port
    /// `downstream`. The downstream port reports a connected device only
    /// once software powers it — mirroring the Pi 4B's onboard
    /// `2109:3431` hub and its keyboard.
    fn with_hub(mem: &SharedMem, ports: u8, downstream: u8) -> Self {
        let mut mock = Self::with_device(mem);
        mock.hub_ports = ports;
        mock.hub_downstream_port = downstream;
        // Current Connect Status (bit 0) | High-Speed Device (bit 10).
        mock.hub_downstream_status = (1 << 0) | (1 << 10);
        mock
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

    /// As [`Self::with_device`], but the controller requires `count`
    /// page-sized scratchpad buffers (the VL805 needs 31) and reports a
    /// 4 KiB page size — and, modelling the real hardware, posts **no**
    /// command completion until software programs `DCBAA[0]`
    /// ([`Self::scratchpad_unprogrammed`]).
    fn with_device_scratchpad(mem: &SharedMem, count: u32) -> Self {
        let mut mock = Self::with_device(mem);
        // Split the count into the HCSPARAMS2 low (bits 31:27) and high
        // (bits 25:21) fields, matching `hcsparams2_max_scratchpad`.
        let lo = count & 0x1F;
        let hi = (count >> 5) & 0x1F;
        mock.hcsparams2 = (lo << 27) | (hi << 21);
        mock.pagesize = 1;
        mock
    }

    /// `true` while a scratchpad-requiring controller's `DCBAA[0]` (the
    /// scratchpad buffer array pointer) is still zero — it executes no
    /// command until software programs it (xHCI §4.20).
    fn scratchpad_unprogrammed(&self) -> bool {
        let dcbaa = Self::qword(self.dcbaap);
        if dcbaa == 0 {
            return true;
        }
        let entry = self.read_dwords(dcbaa, 2);
        entry[0] == 0 && entry[1] == 0
    }

    fn op(offset: usize) -> usize {
        MOCK_CAPLENGTH as usize + offset
    }

    fn ir0(offset: usize) -> usize {
        MOCK_RTSOFF as usize + regs::IR0_BASE + offset
    }

    /// Capture a write to an interrupter-0 register (the event-ring
    /// pointers and the interrupt-management/moderation registers),
    /// returning `true` if `offset` named one. Split out of `write32` to
    /// keep that dispatcher under the line bound.
    fn write_interrupter(&mut self, offset: usize, value: u32) -> bool {
        if offset == Self::ir0(regs::IR_ERSTSZ) {
            self.erstsz = value;
        } else if offset == Self::ir0(regs::IR_ERSTBA) {
            self.erstba[0] = value;
        } else if offset == Self::ir0(regs::IR_ERSTBA) + 4 {
            self.erstba[1] = value;
        } else if offset == Self::ir0(regs::IR_ERDP) {
            self.erdp[0] = value;
        } else if offset == Self::ir0(regs::IR_ERDP) + 4 {
            self.erdp[1] = value;
        } else if offset == Self::ir0(regs::IR_IMAN) {
            // IP (bit 0) is write-1-to-clear; IE (bit 1) is read/write.
            // Clear IP if the write has it set, then store IE.
            if value & regs::IMAN_IP != 0 {
                self.iman &= !regs::IMAN_IP;
            }
            self.iman = (self.iman & regs::IMAN_IP) | (value & regs::IMAN_IE);
        } else if offset == Self::ir0(regs::IR_IMOD) {
            self.imod = value;
        } else {
            return false;
        }
        true
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
        self.post_transfer_event_raw(trb_addr, code.as_u8(), dci, residual);
    }

    /// Post a transfer event explicitly addressed to `slot`, so a test can
    /// model a *trailing* completion the controller posts for a slot the
    /// engine has already freed (after a hot-removal Disable Slot) — which no
    /// longer matches any live endpoint.
    fn post_transfer_event_for_slot(
        &mut self,
        trb_addr: u64,
        code: CompletionCode,
        dci: u8,
        residual: u32,
        slot: u8,
    ) {
        self.post_event(Trb {
            parameter: trb_addr,
            status: (u32::from(code.as_u8()) << 24) | residual,
            control: (u32::from(TrbType::TransferEvent.as_u8()) << 10)
                | (u32::from(dci) << 16)
                | trb::control_slot(slot),
        });
    }

    /// Post a transfer event carrying a *raw* completion-code byte — so
    /// a test can model a controller-specific or reserved code the
    /// driver's [`CompletionCode`] enum does not model (e.g. xHCI code
    /// `7`, Resource Error), which `await_event_for`'s decode rejects.
    fn post_transfer_event_raw(&mut self, trb_addr: u64, code: u8, dci: u8, residual: u32) {
        // A control-endpoint (DCI 1) transfer event belongs to the slot whose
        // EP0 ring is currently live (`ep0_slot`) — the engine keeps a hub and
        // a downstream device addressed at once and switches the active
        // control context between them. Any other endpoint's event belongs to
        // the most-recently-addressed device slot.
        let slot = if dci == 1 {
            self.ep0_slot
        } else {
            self.active_slot
        };
        self.post_event(Trb {
            parameter: trb_addr,
            status: (u32::from(code) << 24) | residual,
            control: (u32::from(TrbType::TransferEvent.as_u8()) << 10)
                | (u32::from(dci) << 16)
                | trb::control_slot(slot),
        });
    }

    /// Post an event carrying an arbitrary *raw* TRB-type (control bits
    /// 15:10) at `trb_addr` — so a test can model an unexpected
    /// asynchronous controller event reaching a transfer/command wait,
    /// which `await_event_for` rejects as an unhandled type.
    fn post_event_raw_type(&mut self, trb_addr: u64, type_raw: u8) {
        self.post_event(Trb {
            parameter: trb_addr,
            status: u32::from(CompletionCode::Success.as_u8()) << 24,
            control: (u32::from(type_raw) << 10) | trb::control_slot(self.active_slot),
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
        // A controller that requires scratchpad buffers does not execute
        // any command until software points `DCBAA[0]` at the scratchpad
        // array (xHCI §4.20) — the VL805's metal `stage=2 completion=0`.
        if regs::hcsparams2_max_scratchpad(self.hcsparams2) > 0 && self.scratchpad_unprogrammed() {
            return;
        }
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
                Ok(TrbType::DisableSlot) => {
                    // Free a device slot on hot-removal (xHCI §6.4.3.3); the
                    // mock just acknowledges it (the engine clears its own
                    // per-device state and DCBAA entry). When
                    // `suppress_disable_completion` is set the controller posts
                    // no completion at all, modelling the metal hot-removal
                    // where the gone device's hub never lets the Disable Slot
                    // be acknowledged.
                    if !self.suppress_disable_completion {
                        self.post_command_completion(addr, CompletionCode::Success, trb.slot_id());
                    }
                }
                Ok(TrbType::ConfigureEndpoint) => {
                    let code = self.handle_configure_endpoint(trb.parameter, trb.slot_id());
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
        // Slot context (the context after the input control context):
        // dword 0 Route String (bits 0:19) + Speed (bits 20:23), dword 2
        // TT Hub Slot ID (bits 0:7) + TT Port Number (bits 8:15).
        let slot_ctx = self.read_dwords(input_ctx + MOCK_CTX_SIZE as u64, 3);
        let route_string = slot_ctx[0] & 0x000F_FFFF;
        let speed = (slot_ctx[0] >> 20) & 0xF;
        let tt_hub_slot = (slot_ctx[2] & 0xFF) as u8;
        let tt_port = ((slot_ctx[2] >> 8) & 0xFF) as u8;
        if route_string != 0 {
            // A device downstream of the hub: validate the Route String
            // and, for a full/low-speed device behind the high-speed hub,
            // the transaction-translator coordinates the driver must
            // program (xHCI §6.2.2 / §8.9). A wrong topology faults
            // Address Device, so the host test proves the driver
            // programmed them — the hub occupies slot 1.
            let route_port = (route_string & 0xF) as u8;
            if route_port != self.hub_downstream_port {
                return CompletionCode::TrbError;
            }
            let needs_tt = speed == 1 || speed == 2;
            let (want_hub, want_port) = if needs_tt {
                (1u8, self.hub_downstream_port)
            } else {
                (0, 0)
            };
            if tt_hub_slot != want_hub || tt_port != want_port {
                return CompletionCode::TrbError;
            }
            self.downstream_active = true;
            self.downstream_route_port = route_port;
        }
        // Save the previously-live slot's EP0 ring progress before this slot
        // becomes the live control context, so switching back to it (e.g. the
        // hub after a downstream device is addressed) resumes where it left
        // off rather than re-reading consumed TRBs.
        let prev = usize::from(self.ep0_slot);
        if prev < self.ep0_saved.len() {
            self.ep0_saved[prev] = (self.ep0_base, self.ep0_index, self.ep0_cycle);
        }
        self.ep0_base = self.ep_ctx_dequeue(input_ctx + 2 * MOCK_CTX_SIZE as u64);
        self.ep0_index = 0;
        self.ep0_cycle = true;
        // This slot's EP0 ring becomes the live control context; record it so
        // a later doorbell for another slot can switch away and back.
        self.ep0_slot = self.active_slot;
        let s = usize::from(self.active_slot);
        if s < self.ep0_saved.len() {
            self.ep0_saved[s] = (self.ep0_base, 0, true);
        }
        self.addressed = true;
        CompletionCode::Success
    }

    fn handle_configure_endpoint(&mut self, input_ctx: u64, slot: u8) -> CompletionCode {
        let control = self.read_dwords(input_ctx, 2);
        let add = control[1];
        // A Configure Endpoint that adds any endpoint (an A(dci) flag
        // beyond the slot-context A0) is the HID endpoint setup; one that
        // names only the slot context (A0 alone) is the hub-topology
        // update that marks the parent hub as a hub.
        let endpoint_adds = add & !0b1;
        if endpoint_adds != 0 {
            // A HID endpoint Configure Endpoint names the slot context
            // (A0) and exactly one endpoint (A(dci)). The DCI is read
            // from the add flags rather than assumed, so a keyboard whose
            // interrupt endpoint is not endpoint 1 is configured at its
            // real DCI (the metal no-report bug was hard-coding DCI 3).
            if add & 0b1 == 0 || endpoint_adds & (endpoint_adds - 1) != 0 {
                return CompletionCode::TrbError;
            }
            let dci = endpoint_adds.trailing_zeros();
            // An endpoint added to the slot already marked a hub is the hub's
            // interrupt-IN status-change endpoint, recorded separately so it
            // does not clobber a downstream device's interrupt endpoint state.
            if self.hub_marked_as_hub && slot == self.hub_slot_id {
                self.hub_int_dci = u8::try_from(dci).expect("DCI fits a byte");
                self.hub_int_base =
                    self.ep_ctx_dequeue(input_ctx + (1 + u64::from(dci)) * MOCK_CTX_SIZE as u64);
                self.hub_int_index = 0;
                self.hub_int_cycle = true;
                return CompletionCode::Success;
            }
            self.int_dci = u8::try_from(dci).expect("DCI fits a byte");
            let ep_ctx_off = input_ctx + (1 + u64::from(dci)) * MOCK_CTX_SIZE as u64;
            let int_ctx = self.read_dwords(ep_ctx_off, 5);
            // Max ESIT Payload Lo (§6.2.3.8 dword 4 bits 16:31): the
            // periodic scheduler reserves no bandwidth when it is zero.
            self.int_max_esit = (int_ctx[4] >> 16) & 0xFFFF;
            self.int_base = self.ep_ctx_dequeue(ep_ctx_off);
            self.int_index = 0;
            self.int_cycle = true;
            self.configured = true;
            return CompletionCode::Success;
        }
        // Hub-topology update (xHCI §6.2.2): the slot context add flag
        // must be set and its Hub bit (dword 0 bit 26) raised — the
        // controller would not route or split transactions to a
        // downstream device otherwise, which is the metal bug where a
        // keyboard behind the hub was addressed but never reported.
        if add & 0b1 == 0 {
            return CompletionCode::TrbError;
        }
        let slot_ctx = self.read_dwords(input_ctx + MOCK_CTX_SIZE as u64, 3);
        if slot_ctx[0] & (1 << 26) == 0 {
            return CompletionCode::TrbError;
        }
        self.hub_marked_as_hub = true;
        self.hub_slot_id = slot;
        self.hub_ctx_num_ports = ((slot_ctx[1] >> 24) & 0xFF) as u8;
        self.hub_ctx_tt_think_time = ((slot_ctx[2] >> 16) & 0b11) as u8;
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

    /// Write `source` into the assembled IN data stage and post a
    /// short-packet event when the device under-fills the TRB — the
    /// shared `GET_DESCRIPTOR` / `GET_STATUS` reply path. Returns
    /// `false` (after posting a `TrbError`) when no data stage was
    /// assembled.
    fn deliver_in_data(
        &mut self,
        data: Option<(u64, u64, u32, bool)>,
        source: &[u8],
        requested_len: usize,
        status_addr: u64,
    ) -> bool {
        let Some((data_addr, buffer, len, isp)) = data else {
            self.post_transfer_event(status_addr, CompletionCode::TrbError, 1, 0);
            return false;
        };
        let requested = usize::min(len as usize, requested_len);
        let supplied = usize::min(requested, source.len());
        self.write_mem(buffer, &source[..supplied]);
        let residual = len - u32::try_from(supplied).expect("reply fits");
        if residual > 0 && isp {
            self.post_transfer_event(data_addr, CompletionCode::ShortPacket, 1, residual);
        }
        true
    }

    /// The `GET_DESCRIPTOR(device | configuration)` fixture to answer with:
    /// the hub fixtures (class `0x09`) while the addressed device is the
    /// hub, the HID keyboard fixtures once a downstream device has been
    /// addressed (a non-zero Route String set `downstream_active`).
    fn descriptor_fixture(&self, desc_type: u8) -> &'static [u8] {
        let is_hub_device = self.hub_ports > 0 && !self.downstream_active;
        match (desc_type, is_hub_device) {
            (0x01, false) => &MOCK_DESCRIPTOR,
            (0x01, true) => &MOCK_HUB_DESCRIPTOR,
            (_, false) => self.keyboard_config,
            (_, true) => &MOCK_HUB_CONFIG_DESCRIPTOR,
        }
    }

    /// Execute the assembled control TD, posting its transfer events.
    fn execute_control(&mut self, status_addr: u64) {
        let Some(setup) = self.pending_setup.take() else {
            self.post_transfer_event(status_addr, CompletionCode::TrbError, 1, 0);
            return;
        };
        // A halted EP0 runs no further transfers until reset (xHCI
        // §4.10.2.4); model that as a transaction error rather than a
        // valid completion.
        if self.ep0_halted {
            self.pending_data.take();
            self.post_transfer_event(status_addr, CompletionCode::UsbTransactionError, 1, 0);
            return;
        }
        let data = self.pending_data.take();
        let w_length = usize::from(u16::from_le_bytes([setup[6], setup[7]]));
        match (setup[0], setup[1]) {
            // GET_DESCRIPTOR(device | configuration); a hub answers with
            // the hub fixtures (class 0x09), a keyboard with the HID ones.
            (0x80, 0x06) if setup[3] == 0x01 || setup[3] == 0x02 => {
                let source = self.descriptor_fixture(setup[3]);
                if !self.deliver_in_data(data, source, w_length, status_addr) {
                    return;
                }
            }
            // Class GET_DESCRIPTOR(hub) (USB 2.0 §11.24.2.5): bDescLength,
            // bDescriptorType=0x29, bNbrPorts, then a minimal tail.
            (0xA0, 0x06) if setup[3] == 0x29 => {
                let desc_type = if self.forge_hub_descriptor {
                    0x00
                } else {
                    0x29
                };
                let hub_desc = [9u8, desc_type, self.hub_ports, 0x00, 0x00, 0x32, 0x00, 0xFF];
                if !self.deliver_in_data(data, &hub_desc, w_length, status_addr) {
                    return;
                }
            }
            // Class SET_FEATURE on a downstream port (USB 2.0 §11.24.2.13):
            // PORT_POWER (8) marks the 1-based port powered.
            (0x23, 0x03) => {
                if setup[4] >= 1 {
                    let bit = 1 << (u32::from(setup[4]) - 1);
                    match setup[2] {
                        // PORT_POWER (8): mark the 1-based port powered.
                        8 => self.hub_powered |= bit,
                        // PORT_RESET (4): mark it reset, so its next
                        // GET_STATUS reports the port enabled — and, like real
                        // hardware, latch the Reset-change bit (wPortChange
                        // bit 4) so the driver must clear it as well as the
                        // connect change or the port stays flagged forever.
                        4 => {
                            self.hub_reset |= bit;
                            if setup[4] == self.hub_downstream_port {
                                self.hub_downstream_change |= 1 << 4;
                            }
                        }
                        _ => {}
                    }
                }
            }
            // Class GET_STATUS on a downstream port (USB 2.0 §11.24.2.7):
            // the connected downstream port reports its status once
            // powered, every other port reads disconnected.
            (0xA3, 0x00) => self.execute_get_port_status(setup[4], data, w_length, status_addr),
            // Class CLEAR_FEATURE on a downstream port (USB 2.0 §11.24.2.2):
            // clear *only* the latched change the feature selector names
            // (C_PORT_CONNECTION=16 .. C_PORT_RESET=20 → wPortChange bits 0..4),
            // mirroring real hardware. A driver that clears only the connect
            // change leaves the reset change (bit 4) latched and the port
            // permanently flagged, so the watch keeps re-firing.
            (0x23, 0x01) => {
                if (16..=20).contains(&setup[2]) {
                    self.hub_downstream_change &= !(1u16 << (setup[2] - 16));
                }
            }
            // SET_CONFIGURATION
            (0x00, 0x09) => self.configuration = Some(setup[2]),
            // SET_PROTOCOL (HID class)
            (0x21, 0x0B) => {
                if self.fault_class_requests {
                    self.post_transfer_event(
                        status_addr,
                        CompletionCode::UsbTransactionError,
                        1,
                        0,
                    );
                    return;
                }
                // A hub is not a HID device, so it STALLs this HID class
                // request — and a STALL halts EP0, exactly the metal
                // failure that breaks a following hub-descriptor read. The
                // downstream device *is* a HID keyboard, so once it is
                // addressed the request succeeds.
                if self.stall_class_requests || (self.hub_ports > 0 && !self.downstream_active) {
                    self.ep0_halted = true;
                    self.post_transfer_event(status_addr, CompletionCode::StallError, 1, 0);
                    return;
                }
                self.protocol = Some(setup[2]);
            }
            _ => {
                self.ep0_halted = true;
                self.post_transfer_event(status_addr, CompletionCode::StallError, 1, 0);
                return;
            }
        }
        self.post_transfer_event(status_addr, CompletionCode::Success, 1, 0);
    }

    fn process_int_ring(&mut self) {
        // A device addressed downstream of the hub receives interrupt
        // transfers only once the controller has been told its parent is
        // a hub (the Hub bit in the hub's slot context, set by a
        // Configure Endpoint). Real hardware never schedules the split
        // transactions otherwise, so the mock delivers no report — the
        // metal bug where the keyboard was addressed but never typed.
        if self.downstream_active && !self.hub_marked_as_hub {
            return;
        }
        // The periodic scheduler reserves no bandwidth for an interrupt
        // endpoint whose Max ESIT Payload is zero (§4.14.2), so the
        // controller services it never and the device delivers no report
        // — the metal bug where the addressed keyboard never typed. A
        // configured interrupt endpoint always carries a non-zero payload
        // once `ep_ctx_dwords` programs it.
        if self.configured && self.int_max_esit == 0 {
            return;
        }
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
            let code = if let Some(bad) = self.fault_one_report_completion.take() {
                // A single odd completion the driver rejects per-report;
                // consumed once so the following report is normal.
                bad
            } else if residual > 0 {
                CompletionCode::ShortPacket
            } else {
                CompletionCode::Success
            };
            self.post_transfer_event(addr, code, self.int_dci, residual);
        }
    }

    /// Deliver one hub status-change report: write `bitmap` (the port-change
    /// bitmap, USB 2.0 §11.12.4) into the armed status-change transfer's
    /// buffer and post its completion on the hub slot's status-change
    /// endpoint, so the engine's `next_hub_change` wakes and services it.
    ///
    /// Mirrors [`Self::process_int_ring`] for the hub's interrupt-IN
    /// status-change endpoint; the event carries the hub's slot id and DCI so
    /// the engine routes it as a hub completion, never a keyboard report.
    fn post_hub_status_change(&mut self, bitmap: &[u8]) {
        let (mut index, mut cycle) = (self.hub_int_index, self.hub_int_cycle);
        let base = self.hub_int_base;
        let Some((addr, trb)) = self.next_owned(base, &mut index, &mut cycle) else {
            return;
        };
        if trb.trb_type() != Ok(TrbType::Normal) {
            return;
        }
        self.hub_int_index = index;
        self.hub_int_cycle = cycle;
        self.write_mem(trb.parameter, bitmap);
        let residual = trb.status - u32::try_from(bitmap.len()).expect("bitmap fits");
        let code = if residual > 0 {
            CompletionCode::ShortPacket
        } else {
            CompletionCode::Success
        };
        self.post_event(Trb {
            parameter: addr,
            status: (u32::from(code.as_u8()) << 24) | residual,
            control: (u32::from(TrbType::TransferEvent.as_u8()) << 10)
                | (u32::from(self.hub_int_dci) << 16)
                | trb::control_slot(self.hub_slot_id),
        });
    }

    /// Execute a class `GET_STATUS` on downstream hub `port` (USB 2.0
    /// §11.24.2.7): honour the fault knobs, then reply with the port's
    /// `wPortStatus` (connect/speed once powered, plus enabled once reset) and
    /// its latched `wPortChange`.
    fn execute_get_port_status(
        &mut self,
        port: u8,
        data: Option<(u64, u64, u32, bool)>,
        w_length: usize,
        status_addr: u64,
    ) {
        if self.fault_hub_port_status {
            self.post_transfer_event(status_addr, CompletionCode::StallError, 1, 0);
            return;
        }
        if self.fault_hub_port_status_raw != 0 {
            self.post_transfer_event_raw(status_addr, self.fault_hub_port_status_raw, 1, 0);
            return;
        }
        if self.fault_hub_port_status_evtype != 0 {
            self.post_event_raw_type(status_addr, self.fault_hub_port_status_evtype);
            return;
        }
        let bit = if port >= 1 {
            1 << (u32::from(port) - 1)
        } else {
            0
        };
        let powered = port >= 1 && self.hub_powered & bit != 0;
        let w_status = if powered && port == self.hub_downstream_port {
            // Once the port has been reset it reports enabled
            // (PORT_STATUS_ENABLE, bit 1) in addition to its connect/speed bits.
            let enabled = if self.hub_reset & bit != 0 { 1 << 1 } else { 0 };
            self.hub_downstream_status | enabled
        } else {
            0
        };
        // The latched `wPortChange` (e.g. Connect Status Change) is reported
        // for the watched downstream port, so the hub-hotplug path can confirm
        // and clear it.
        let change = if port == self.hub_downstream_port {
            self.hub_downstream_change
        } else {
            0
        };
        let status_bytes = w_status.to_le_bytes();
        let change_bytes = change.to_le_bytes();
        let reply = [
            status_bytes[0],
            status_bytes[1],
            change_bytes[0],
            change_bytes[1],
        ];
        self.deliver_in_data(data, &reply, w_length, status_addr);
    }

    /// Reset the device-model ring consumer positions and per-slot state, as a
    /// Host Controller Reset does on real hardware (xHCI §4.2): every slot,
    /// ring dequeue position, and addressed/configured state is cleared, so a
    /// re-bring-up re-programs the rings and re-enumerates from scratch rather
    /// than reading a ring from a stale dequeue position.
    fn reset_device_model(&mut self) {
        self.cmd_index = 0;
        self.cmd_cycle = true;
        self.ep0_index = 0;
        self.ep0_cycle = true;
        self.ep0_slot = 0;
        self.ep0_saved = [(0, 0, true); 33];
        self.int_index = 0;
        self.int_cycle = true;
        self.event_index = 0;
        self.event_cycle = true;
        self.next_slot = 1;
        self.active_slot = 0;
        self.addressed = false;
        self.configured = false;
        self.downstream_active = false;
        self.downstream_route_port = 0;
        self.hub_marked_as_hub = false;
        self.hub_slot_id = 0;
        self.hub_int_base = 0;
        self.hub_int_dci = 0;
        self.hub_reset = 0;
        self.hub_powered = 0;
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
        if offset == regs::HCSPARAMS2 {
            return Ok(self.hcsparams2);
        }
        if offset == regs::HCCPARAMS1 {
            return Ok(self.hccparams1);
        }
        if offset == Self::op(regs::PAGESIZE) {
            return Ok(self.pagesize);
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
            if self.pending_status_clear & regs::USBSTS_HSE != 0 {
                self.hse_latched = false;
            }
            if self.pending_status_clear & regs::USBSTS_EINT != 0 {
                self.eint_latched = false;
            }
            if self.pending_status_clear & regs::USBSTS_PCD != 0 {
                self.pcd_latched = false;
            }
            self.pending_status_clear = 0;
            let mut status = 0;
            if self.cnr_stuck || self.cnr_reads > 0 {
                self.cnr_reads = self.cnr_reads.saturating_sub(1);
                status |= regs::USBSTS_CNR;
            }
            if self.usbcmd & regs::USBCMD_RUN == 0 {
                status |= regs::USBSTS_HCH;
            }
            if self.hse_latched {
                status |= regs::USBSTS_HSE;
            }
            if self.eint_latched {
                status |= regs::USBSTS_EINT;
            }
            if self.pcd_latched {
                status |= regs::USBSTS_PCD;
            }
            return Ok(status);
        }
        if offset == Self::op(regs::CONFIG) {
            return Ok(self.config);
        }
        if offset == Self::ir0(regs::IR_IMAN) {
            return Ok(self.iman);
        }
        if offset == Self::ir0(regs::IR_IMOD) {
            return Ok(self.imod);
        }
        if offset == Self::ir0(regs::IR_ERSTSZ) {
            return Ok(self.erstsz);
        }
        if offset == Self::ir0(regs::IR_ERDP) {
            return Ok(self.erdp[0]);
        }
        if offset == Self::ir0(regs::IR_ERDP) + 4 {
            return Ok(self.erdp[1]);
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
                self.hcrst_stuck |= self.hse_latched || self.pcd_latched;
                self.cnr_reads = 0;
                self.reset_device_model();
            }
            return Ok(());
        }
        if offset == Self::op(regs::USBSTS) {
            let clear = value & (regs::USBSTS_HSE | regs::USBSTS_EINT | regs::USBSTS_PCD);
            if self.status_write_needs_read_flush {
                self.pending_status_clear |= clear;
            } else {
                if clear & regs::USBSTS_HSE != 0 {
                    self.hse_latched = false;
                }
                if clear & regs::USBSTS_EINT != 0 {
                    self.eint_latched = false;
                }
                if clear & regs::USBSTS_PCD != 0 {
                    self.pcd_latched = false;
                }
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
        if self.write_interrupter(offset, value) {
            return Ok(());
        }
        let portsc_base = Self::op(regs::PORTSC_BASE);
        for port in 0..self.portsc.len() {
            if offset == portsc_base + port * regs::PORTSC_STRIDE {
                if value & regs::PORTSC_PP != 0 {
                    // Port Power latches sticky, as on a controller whose
                    // ports software powers on (xHCI 1.2 §5.4.8).
                    self.portsc[port] |= regs::PORTSC_PP;
                    // A port-power-controlled controller (PPC = 1) only
                    // reports a device once the port is powered: a latent
                    // device asserts Current Connect Status here.
                    if self.latent_device_port == Some(port) {
                        self.portsc[port] |= regs::PORTSC_CCS | (3 << regs::PORTSC_SPEED_SHIFT);
                    }
                }
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
                self.ring_doorbell_model((offset - db_base) / 4, value);
            }
            return Ok(());
        }
        Ok(())
    }
}

impl MockXhci {
    /// Service a doorbell write at slot `index` with target `value` (the DCI,
    /// or `0` for the command ring), driving the matching ring's device model.
    fn ring_doorbell_model(&mut self, index: usize, value: u32) {
        match (index, value) {
            (0, 0) => self.process_command_ring(),
            (_, 1) => {
                // Switch the live EP0 ring to the rung slot's, like the
                // DCBAA-indexed hardware: save the current slot's ring state
                // and load the rung slot's.
                if index < self.ep0_saved.len() && u8::try_from(index) != Ok(self.ep0_slot) {
                    let cur = usize::from(self.ep0_slot);
                    if cur < self.ep0_saved.len() {
                        self.ep0_saved[cur] = (self.ep0_base, self.ep0_index, self.ep0_cycle);
                    }
                    let (base, idx, cycle) = self.ep0_saved[index];
                    self.ep0_base = base;
                    self.ep0_index = idx;
                    self.ep0_cycle = cycle;
                    self.ep0_slot = u8::try_from(index).unwrap_or(0);
                }
                self.process_ep0_ring();
            }
            (_, 3) => self.process_int_ring(),
            _ => {}
        }
    }
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
fn open_resets_a_halted_controller_with_pre_reset_cnr_and_hse() {
    let mut mock = MockXhci::new();
    mock.cnr_reads = 128;
    mock.hse_latched = true;
    mock.pcd_latched = true;
    let mut xhci = Xhci::open_with_budget(mock, 16).expect("reset clears stale pre-reset status");

    let status = xhci.host.read32(MockXhci::op(regs::USBSTS)).unwrap();
    assert_eq!(
        status & (regs::USBSTS_CNR | regs::USBSTS_HSE | regs::USBSTS_PCD),
        0
    );
}

#[test]
fn open_flushes_pre_reset_status_clear_before_hcrst() {
    let mut mock = MockXhci::new();
    mock.hse_latched = true;
    mock.pcd_latched = true;
    mock.status_write_needs_read_flush = true;

    let mut xhci = Xhci::open_with_budget(mock, 16).expect("status clear is flushed before reset");
    let usbcmd = xhci.host.read32(MockXhci::op(regs::USBCMD)).unwrap();
    let usbsts = xhci.host.read32(MockXhci::op(regs::USBSTS)).unwrap();

    assert_eq!(usbcmd & regs::USBCMD_HCRST, 0);
    assert_eq!(usbsts & (regs::USBSTS_HSE | regs::USBSTS_PCD), 0);
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
fn open_diagnostic_reports_the_stuck_reset_stage() {
    let mut mock = MockXhci::new();
    mock.hcrst_stuck = true;
    let Err(err) = Xhci::open_diagnostic_with_budget(mock, 16) else {
        panic!("reset must time out")
    };

    assert_eq!(err.error, DriverError::DeviceFault);
    assert_eq!(err.stage, XhciOpenStage::ResetSelfClear);
    assert_eq!(err.registers.usbcmd, Some(regs::USBCMD_HCRST));
    assert_eq!(err.registers.usbsts, Some(regs::USBSTS_HCH));
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
fn event_cursor_owned_peeks_without_advancing() {
    // `owned` reports producer ownership by the cycle bit alone and must not
    // advance the cursor — `poll_event` relies on this to read the cycle, then
    // `dma_rmb`, then re-read and `pop` the entry body (the torn-read fix for
    // non-coherent DMA).
    let mut segment = [Trb::ZERO; 3];
    let mut cursor = EventRingCursor::new(3).expect("segment fits");
    assert_eq!(cursor.owned(&segment), Ok(false), "nothing produced yet");
    segment[0] = Trb::new(
        TrbType::CommandCompletion,
        0x1000,
        u32::from(CompletionCode::Success.as_u8()) << 24,
        CONTROL_CYCLE,
    );
    assert_eq!(cursor.owned(&segment), Ok(true), "producer owns slot 0 now");
    // Peeking twice still does not advance: a following `pop` consumes it.
    assert_eq!(cursor.owned(&segment), Ok(true));
    assert_eq!(cursor.dequeue_index(), 0, "peek left the cursor put");
    assert!(cursor.pop(&segment).unwrap().is_some());
    assert_eq!(cursor.dequeue_index(), 1);
    // A wrong-length segment is rejected like `pop`.
    assert_eq!(
        cursor.owned(&[Trb::ZERO; 4]),
        Err(DriverError::LengthOutOfRange)
    );
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

fn arm_report_request(device: &mut UsbDevice<MockXhci, MockDma>) {
    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(
        device.next_report(&mut buf),
        Ok(None),
        "a class report request arms one interrupt-IN transfer and then parks"
    );
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
fn hcsparams2_decodes_the_vl805_scratchpad_count() {
    // VL805 datasheet HCSPARAMS2 default `FC000031h` → 31 scratchpad
    // buffers (low field bits 31:27 = 0x1F, high field bits 25:21 = 0).
    assert_eq!(regs::hcsparams2_max_scratchpad(0xFC00_0031), 31);
    // A high-field-only value combines into the 10-bit count.
    assert_eq!(regs::hcsparams2_max_scratchpad(1 << 21), 32);
    // No scratchpad required.
    assert_eq!(regs::hcsparams2_max_scratchpad(0), 0);
}

#[test]
fn pagesize_decodes_the_lowest_supported_page() {
    // Bit 0 → 4 KiB (the VL805's page); a higher bit → its `2^(n+12)`.
    assert_eq!(regs::pagesize_bytes(1), 4096);
    assert_eq!(regs::pagesize_bytes(1 << 4), 1 << 16);
    // An unset register reports no size, so the caller fails closed.
    assert_eq!(regs::pagesize_bytes(0), 0);
}

#[test]
fn start_reserves_scratchpad_and_programs_dcbaa0() {
    // A VL805-shaped controller: 31 page-sized scratchpad buffers, and
    // no command completes until software points `DCBAA[0]` at the
    // scratchpad array (xHCI §4.20). Before this fix the very first
    // Enable Slot produced no completion event (the Pi 4 metal
    // `4126 stage=2 completion=0`); now `start` reserves the buffers, so
    // the command ring runs and enumeration completes.
    let mem: SharedMem = Rc::new(RefCell::new(alloc::vec![0u8; 256 * 1024]));
    let xhci = Xhci::open(MockXhci::with_device_scratchpad(&mem, 31)).expect("bring-up succeeds");
    assert_eq!(xhci.max_scratchpad_buffers(), 31);
    assert_eq!(xhci.page_size(), 4096);
    let dma = MockDma {
        mem: Rc::clone(&mem),
        phys: MOCK_DMA_BASE,
    };
    let mut device = UsbDevice::start(xhci, dma, 4096).expect("engine starts with scratchpad");

    // `DCBAA[0]` now points at a non-zero scratchpad pointer array...
    let dcbaa_base = MockXhci::qword(device.host_mut().dcbaap);
    let array = device.host_mut().read_dwords(dcbaa_base, 2);
    let array_ptr = (u64::from(array[1]) << 32) | u64::from(array[0]);
    assert_ne!(array_ptr, 0, "DCBAA[0] points at the scratchpad array");
    // ...whose first entry is a non-zero, page-aligned scratchpad buffer.
    let entry = device.host_mut().read_dwords(array_ptr, 2);
    let page0 = (u64::from(entry[1]) << 32) | u64::from(entry[0]);
    assert_ne!(page0, 0, "scratchpad array entry 0 points at a buffer");
    assert_eq!(page0 % 4096, 0, "scratchpad buffers are page-aligned");

    // And a command actually completes now: enumeration runs end to end.
    let descriptor = device
        .enumerate_hid(1)
        .expect("enumeration completes once the scratchpad is reserved");
    assert_eq!(descriptor.vendor_id, 0x046D);
}

#[test]
fn start_stalls_without_scratchpad_on_a_controller_that_needs_it() {
    // The same VL805-shaped controller, but the engine is denied a region
    // large enough to reserve the 31 scratchpad pages: `start` fails
    // closed (`LengthOutOfRange`) rather than running a controller whose
    // `DCBAA[0]` it could not program.
    let small: SharedMem = Rc::new(RefCell::new(alloc::vec![0u8; 0x4000]));
    let xhci = Xhci::open(MockXhci::with_device_scratchpad(&small, 31)).expect("bring-up succeeds");
    let dma = MockDma {
        mem: Rc::clone(&small),
        phys: MOCK_DMA_BASE,
    };
    assert_eq!(
        UsbDevice::start(xhci, dma, 4096).err(),
        Some(DriverError::LengthOutOfRange)
    );
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
    // `NotFound` rather than guessing a port.
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
fn set_port_power_asserts_pp_and_rejects_a_bad_port() {
    // A port-power-controlled controller reports a port unpowered after
    // the open-time Host Controller Reset; `set_port_power` asserts `PP`
    // (xHCI 1.2 §4.19.1.1 / §5.4.8).
    let mut mock = MockXhci::new();
    mock.portsc[0] = 0;
    let mut xhci = Xhci::open(mock).expect("bring-up succeeds");
    assert_eq!(xhci.port_status(1).unwrap().raw() & regs::PORTSC_PP, 0);
    xhci.set_port_power(1).expect("port 1 powers on");
    assert_ne!(xhci.port_status(1).unwrap().raw() & regs::PORTSC_PP, 0);
    // Idempotent on an already-powered port; out-of-range fails closed.
    xhci.set_port_power(1)
        .expect("powering an on port is a no-op");
    assert_eq!(xhci.set_port_power(0), Err(DriverError::OutOfRange));
    assert_eq!(xhci.set_port_power(99), Err(DriverError::OutOfRange));
}

#[test]
fn enumerate_first_connected_powers_every_root_port() {
    // The scan must power on every reported port before reading connect
    // status, or a port-power-controlled controller hides attached
    // devices. Start with all ports unpowered (the post-reset shape) and
    // confirm each carries `PP` afterwards.
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    for port in 0..mock.portsc.len() {
        mock.portsc[port] &= !regs::PORTSC_PP;
    }
    let mut device = started_device(mock, &mem);
    device
        .enumerate_first_connected()
        .expect("port 1 is connected once powered");
    let mock = device.host_mut();
    for port in 0..mock.portsc.len() {
        assert_ne!(
            mock.portsc[port] & regs::PORTSC_PP,
            0,
            "root-hub port {port} was powered on"
        );
    }
}

#[test]
fn enumerate_first_connected_connects_a_port_only_after_power() {
    // Model the VL805: the device reports no Current Connect Status until
    // software powers the port. A scan that read connect status without
    // first asserting `PP` (the old behaviour) would find nothing; the
    // power-then-debounce scan brings the device up.
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    mock.portsc[0] = 0;
    mock.latent_device_port = Some(0);
    let mut device = started_device(mock, &mem);
    let descriptor = device
        .enumerate_first_connected()
        .expect("the device appears once its port is powered");
    assert_eq!(descriptor.vendor_id, 0x046D);
    assert_eq!(device.slot(), 1);
    assert!(device.host_mut().configured);
}

#[test]
fn root_port_status_raw_reports_each_port_and_rejects_a_bad_port() {
    // The diagnostic accessor walks every reported port and fails closed
    // on an out-of-range port.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    assert_eq!(device.root_port_count(), 4);
    let raw = device.root_port_status_raw(1).expect("port 1 reads");
    assert_ne!(raw & regs::PORTSC_CCS, 0, "port 1 has the connected device");
    assert_eq!(device.root_port_status_raw(0), Err(DriverError::OutOfRange));
    assert_eq!(
        device.root_port_status_raw(99),
        Err(DriverError::OutOfRange)
    );
}

#[test]
fn enumerate_hid_tolerates_a_stalled_set_protocol() {
    // `SET_PROTOCOL(boot)` is optional (HID 1.11 §7.2.6): a device that
    // does not implement it STALLs, which is a protocol stall the
    // default control endpoint recovers from. The Pi 4 VL805 keyboard
    // does exactly this (metal `4126 stage=8 completion=6`); the engine
    // must absorb it and finish enumeration rather than aborting an
    // otherwise-usable keyboard, leaving the device in its default
    // protocol (the mock therefore never records a selected protocol).
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    mock.stall_class_requests = true;
    let mut device = started_device(mock, &mem);
    let descriptor = device
        .enumerate_hid(1)
        .expect("a stalled SET_PROTOCOL is tolerated");
    assert_eq!(descriptor.vendor_id, 0x046D);
    assert_eq!(device.enum_stage(), EnumStage::Configured);
    // The STALL was observed (the diagnostic preserves it) but absorbed.
    assert_eq!(
        device.last_completion_code(),
        CompletionCode::StallError.as_u8()
    );
    assert_eq!(
        device.host_mut().protocol,
        None,
        "the stalled request selected no protocol"
    );
}

#[test]
fn enumerate_hid_records_the_configured_stage_on_success() {
    // A clean enumeration walks the breadcrumb to `Configured`, and the
    // last completion observed is the SET_PROTOCOL status stage's
    // Success — the fault-localising diagnostic reads a healthy run.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    assert_eq!(device.enum_stage(), EnumStage::Scan);
    device.enumerate_hid(1).expect("enumeration succeeds");
    assert_eq!(device.enum_stage(), EnumStage::Configured);
    assert_eq!(
        device.last_completion_code(),
        CompletionCode::Success.as_u8()
    );
}

#[test]
fn enumerate_hid_fails_closed_on_a_non_stall_class_fault() {
    // A STALL on the optional SET_PROTOCOL is tolerated, but a *genuine*
    // class-request fault (here a USB transaction error) is not optional
    // — it still fails closed, leaving the breadcrumb
    // at exactly that step with the raw completion code so a metal
    // capture pins the faulting xHCI operation.
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    mock.fault_class_requests = true;
    let mut device = started_device(mock, &mem);
    assert_eq!(device.enumerate_hid(1), Err(DriverError::DeviceFault));
    assert_eq!(device.enum_stage(), EnumStage::SetProtocol);
    assert_eq!(
        device.last_completion_code(),
        CompletionCode::UsbTransactionError.as_u8()
    );
}

#[test]
fn enumerate_hid_flags_a_hub_via_the_device_class() {
    // The Pi 4B's onboard 2109:3431 VIA Labs hub enumerates on root-hub
    // port 1; the keyboard hangs off it, so the bring-up must recognise
    // the enumerated device is a hub (bDeviceClass 0x09) rather than
    // treating it as the keyboard (metal `4102 vendor=2109 product=3431`).
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_hub(&mem, 4, 2), &mem);
    let descriptor = device.enumerate_hid(1).expect("the hub enumerates");
    assert_eq!(descriptor.vendor_id, 0x2109);
    assert_eq!(descriptor.product_id, 0x3431);
    assert!(descriptor.is_hub(), "device class 0x09 is recognised");
}

#[test]
fn enumerating_a_hub_leaves_ep0_usable_for_the_hub_descriptor() {
    // A hub is not a HID device: issuing the HID `SET_PROTOCOL(boot)` to
    // it STALLs, and an xHCI STALL halts the control endpoint, so a
    // following hub-descriptor read on EP0 faults (the metal `reading
    // the hub descriptor failed err=device_fault`). The bring-up must
    // therefore not send `SET_PROTOCOL` to a non-HID interface; this
    // asserts the hub never selects a protocol, EP0 stays unhalted, and
    // the hub-descriptor read succeeds. It fails before that gate.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_hub(&mem, 4, 2), &mem);
    device.enumerate_hid(1).expect("the hub enumerates");

    assert_eq!(
        device.host_mut().protocol,
        None,
        "a hub is not sent the HID SET_PROTOCOL request"
    );
    assert!(
        !device.host_mut().ep0_halted,
        "EP0 is never STALL-halted enumerating a hub"
    );
    assert_eq!(
        device
            .hub_num_ports()
            .expect("hub descriptor read succeeds"),
        4,
        "the hub-descriptor read runs on a usable EP0"
    );
}

#[test]
fn hub_discovery_finds_the_downstream_device() {
    // After the hub enumerates, reading its descriptor reports the
    // downstream port count, and — once every downstream port is
    // powered — GET_STATUS reports the keyboard's port connected at its
    // speed, while an unpopulated port reads disconnected.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_hub(&mem, 4, 2), &mem);
    device.enumerate_hid(1).expect("the hub enumerates");

    assert_eq!(device.hub_num_ports().expect("hub descriptor read"), 4);
    for port in 1..=4 {
        device
            .power_hub_port(port)
            .expect("power the downstream port");
    }
    let status = device.hub_port_status(2).expect("downstream port status");
    assert!(
        hub_port_connected(status),
        "the keyboard's port is connected"
    );
    assert_eq!(
        hub_port_speed(status),
        3,
        "the downstream device is high-speed"
    );

    let empty = device.hub_port_status(1).expect("downstream port status");
    assert!(
        !hub_port_connected(empty),
        "an unpopulated downstream port reads disconnected"
    );
}

#[test]
fn hub_port_reads_disconnected_until_powered() {
    // A port-power-controlled hub reports a downstream port
    // disconnected until software sets PORT_POWER (USB 2.0 §11.11), so
    // an unpowered scan finds nothing — mirroring the root-hub path.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_hub(&mem, 4, 2), &mem);
    device.enumerate_hid(1).expect("the hub enumerates");

    let before = device.hub_port_status(2).expect("downstream port status");
    assert!(
        !hub_port_connected(before),
        "the downstream port reads disconnected before power"
    );
    device.power_hub_port(2).expect("power the downstream port");
    let after = device.hub_port_status(2).expect("downstream port status");
    assert!(
        hub_port_connected(after),
        "the downstream port connects once powered"
    );
}

#[test]
fn enumerating_a_hub_does_not_arm_its_interrupt_endpoint() {
    // A hub has an interrupt status-change endpoint, but this engine
    // never reads it — a hub's downstream ports are polled over EP0
    // hub-class GET_STATUS. Arming it (as the keyboard path does) makes
    // a real hub deliver asynchronous status-change reports that
    // interleave with — and fail — those EP0 control transfers: the
    // controller posts a transfer event for the interrupt TRB, whose
    // pointer is not in the control wait's watch list, so the wait
    // rejects it (REJECT_ADDRESS_MISMATCH) and the faulted transfer
    // leaves the ring wedged (the metal `4127` all-ones `0xffff` reads
    // with `completion=0xd`/`reject=2` on the first ports and no event
    // at all on the rest). Model the hub with a status-change report
    // queued on its interrupt endpoint: because the bring-up never
    // configures or doorbells that endpoint for a hub, the report is
    // never delivered, no async event contaminates EP0, and every
    // hub-class read still succeeds. Fails before the fix (the first
    // hub-class read trips the mismatch); passes after.
    //
    // The interrupt-IN endpoint's doorbell value is `DCI_INTERRUPT_IN`
    // (3, a private const); the control/command doorbells use 1/0.
    const DCI_INTERRUPT_IN_DB: u32 = 3;
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 2);
    mock.pending_reports.push_back(alloc::vec![0x02]);
    let mut device = started_device(mock, &mem);
    device.enumerate_hid(1).expect("the hub enumerates");

    assert!(
        !device
            .host_mut()
            .doorbells
            .iter()
            .any(|&(_, value)| value == DCI_INTERRUPT_IN_DB),
        "a hub's interrupt-IN endpoint is never doorbelled"
    );

    assert_eq!(
        device
            .hub_num_ports()
            .expect("hub descriptor read succeeds"),
        4,
    );
    for port in 1..=4 {
        device
            .power_hub_port(port)
            .expect("power the downstream port");
    }
    let status = device
        .hub_port_status(2)
        .expect("downstream port status read succeeds despite the queued report");
    assert!(
        hub_port_connected(status),
        "the keyboard's downstream port is connected"
    );
}

#[test]
fn enumerate_downstream_hid_addresses_a_full_speed_keyboard_through_the_hub() {
    // The Pi 4B metal case: the onboard 2109:3431 hub enumerates on slot
    // 1, and a *full-speed* keyboard hangs off a downstream port (the
    // metal `4127` capture: connected, no speed bit → full speed). Reach
    // it on a second xHCI slot whose slot context carries the Route
    // String (the downstream port) and — because a full-speed device
    // behind a high-speed hub must split its transactions — the TT Hub
    // Slot ID (the hub's slot) and TT Port Number (xHCI §6.2.2 / §8.9).
    // The mock faults Address Device unless those are programmed exactly,
    // so reaching the keyboard descriptor proves the driver built them.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    // A full-speed downstream device: Current Connect Status only, no
    // High-Speed bit (the metal `wstatus 0x0101` after power: connect +
    // power, no speed bit).
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);

    let hub = device.enumerate_hid(1).expect("the hub enumerates");
    assert!(hub.is_hub(), "the device on the root hub is the VIA hub");
    let hub_slot = device.slot();

    // Bring the keyboard's downstream port up: power, reset, confirm
    // enabled (the caller owns these wall-clock delays on metal).
    device.power_hub_port(4).expect("power the downstream port");
    assert!(
        hub_port_connected(device.hub_port_status(4).expect("status")),
        "the keyboard's port is connected once powered"
    );
    device.reset_hub_port(4).expect("reset the downstream port");
    let status = device.hub_port_status(4).expect("status after reset");
    assert!(
        hub_port_enabled(status),
        "the downstream port is enabled after reset"
    );
    let speed = hub_port_speed(status);
    assert_eq!(speed, 1, "the keyboard reports full speed behind the hub");

    let keyboard = device
        .enumerate_downstream_hid(4, speed)
        .expect("the keyboard behind the hub is addressed and configured");
    assert!(
        !keyboard.is_hub(),
        "the downstream device is the HID keyboard, not another hub"
    );
    assert_eq!(keyboard.vendor_id, 0x046D);
    assert_eq!(keyboard.product_id, 0xC077);

    // The keyboard occupies a *second* slot, distinct from the hub's,
    // and the engine is now pointed at it.
    let kbd_slot = device.slot();
    assert_ne!(kbd_slot, hub_slot, "the keyboard gets its own slot");
    assert_eq!(kbd_slot, 2);

    // The mock validated and recorded the Route String it was addressed
    // with — the hub's downstream port.
    assert_eq!(device.host_mut().downstream_route_port, 4);

    // The keyboard's HID interface is captured for the hardware-tree
    // child node, and a class report request drains after the controller
    // completes it.
    let node = device
        .describe_device(0, 1)
        .expect("the keyboard describes a child node");
    assert_eq!(node.class(), Some(rustos_abi::HwDeviceClass::Input));
    arm_report_request(&mut device);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,]);
    device.host_mut().process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    let len = device
        .next_report(&mut buf)
        .expect("a report drains")
        .expect("a report is available");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04, "the 'a' keycode reaches the report buffer");
}

/// A deterministic [`Delay`] for the host tests: counts `delay_us`
/// invocations and advances a synthetic monotonic clock, so a test asserts
/// the hub settle windows were honoured without sleeping (no flaky tests).
#[derive(Default)]
struct TestDelay {
    calls: core::cell::Cell<u32>,
    now: core::cell::Cell<u64>,
}

impl Delay for TestDelay {
    fn delay_us(&self, us: u32) {
        self.calls.set(self.calls.get() + 1);
        self.now.set(self.now.get() + u64::from(us));
    }

    fn now_us(&self) -> u64 {
        self.now.get()
    }
}

#[test]
fn bring_up_keyboard_returns_a_directly_attached_keyboard() {
    // A keyboard wired straight to a root-hub port (no intervening hub):
    // the orchestration enumerates the first connected port and, because
    // the device is not a hub, returns it without touching the clock.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    let delay = TestDelay::default();

    let descriptor = match device
        .bring_up_keyboard(&delay)
        .expect("the directly-attached keyboard enumerates")
    {
        BringUp::Device(descriptor) => descriptor,
        BringUp::AwaitingDevice => panic!("a directly-attached keyboard must enumerate now"),
    };
    assert!(device.device_present(), "the enumerated device is live");
    assert!(!descriptor.is_hub(), "the root-port device is the keyboard");
    assert_eq!(descriptor.vendor_id, 0x046D);
    assert_eq!(
        delay.calls.get(),
        0,
        "no hub tier means no settle window is waited"
    );

    // Its boot report drains after the class side asks for one.
    arm_report_request(&mut device);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
    device.host_mut().process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    let len = device
        .next_report(&mut buf)
        .expect("a report drains")
        .expect("a report is available");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04, "the 'a' keycode reaches the report buffer");
}

#[test]
fn bring_up_keyboard_descends_through_a_hub_to_the_keyboard() {
    // The Pi 4B metal topology: the onboard hub enumerates on the root
    // port and a full-speed keyboard hangs off a downstream port. The
    // orchestration recognises the hub, powers its ports, waits the
    // power-on-good window, resets the connected port, waits reset
    // recovery, and addresses the keyboard on a second slot — without the
    // caller naming a port (discovered, not guessed).
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    // Full-speed downstream device (the metal `wstatus` case: connect, no
    // high-speed bit), so its transactions split through the hub's TT.
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    let keyboard = match device
        .bring_up_keyboard(&delay)
        .expect("the keyboard behind the onboard hub is reached")
    {
        BringUp::Device(keyboard) => keyboard,
        BringUp::AwaitingDevice => panic!("a connected downstream keyboard must enumerate now"),
    };
    assert!(
        !keyboard.is_hub(),
        "the downstream device is the HID keyboard, not another hub"
    );
    assert_eq!(keyboard.vendor_id, 0x046D);
    assert_eq!(keyboard.product_id, 0xC077);
    // Descended one tier: the keyboard sits on a second xHCI slot,
    // addressed through the hub's downstream port 4.
    assert_eq!(device.slot(), 2, "the keyboard gets its own slot");
    assert_eq!(device.host_mut().downstream_route_port, 4);
    // Both hardware settle windows were honoured (power-on-good then
    // reset-recovery), each exactly once.
    assert_eq!(delay.calls.get(), 2);

    // With the hub marked and the endpoint configured, a requested report drains.
    arm_report_request(&mut device);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
    device.host_mut().process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    let len = device
        .next_report(&mut buf)
        .expect("a report drains")
        .expect("a report is available");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04);
}

#[test]
fn bring_up_keyboard_arms_the_hub_watch_when_no_downstream_device_is_present() {
    // The root device is the onboard hub, but no downstream port has a
    // device yet (a cold boot with the keyboard unplugged). Bring-up must
    // NOT fail: the controller comes up, the hub's status-change watch is
    // armed, and `AwaitingDevice` is returned so the HCD waits for the first
    // connect event rather than failing closed.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    // No connect bit, so every downstream port reads disconnected even
    // after it is powered.
    mock.hub_downstream_status = 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    assert_eq!(
        device.bring_up_keyboard(&delay),
        Ok(BringUp::AwaitingDevice),
        "a hub with nothing attached downstream comes up awaiting a device"
    );
    assert!(
        device.hub_watch_active(),
        "the hub status-change watch is armed so the first connect is delivered event-driven"
    );
    assert!(
        !device.device_present(),
        "no HID device is live until one connects downstream"
    );
    // The power-on-good window was waited once; the reset-recovery wait is
    // never reached because no connected port is found.
    assert_eq!(delay.calls.get(), 1);
}

#[test]
fn bring_up_keyboard_then_a_downstream_connect_enumerates_a_fresh_keyboard() {
    // The cold-boot hot-plug path: the controller comes up with the onboard
    // hub present but no downstream device (`AwaitingDevice`, watch armed),
    // then a keyboard is plugged into a downstream port. A hub status-change
    // report drives `next_hub_change` to enumerate it as a brand-new device,
    // exactly as a re-attach would, and the keyboard's reports then drain.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 0; // nothing attached downstream at boot
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    assert_eq!(
        device.bring_up_keyboard(&delay),
        Ok(BringUp::AwaitingDevice),
        "cold boot with no downstream device comes up awaiting one"
    );
    assert!(device.hub_watch_active());

    // A full-speed keyboard is now plugged into downstream port 4: the hub
    // latches a connect change and posts a status-change report naming that
    // port (bit 4 of the change bitmap).
    device.host_mut().hub_downstream_status = 1 << 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);

    let descriptor = match device
        .next_hub_change(&delay)
        .expect("the status-change report is serviced")
    {
        HubEvent::Attached(descriptor) => descriptor,
        other => panic!("a downstream connect must enumerate a device, got {other:?}"),
    };
    assert!(
        !descriptor.is_hub(),
        "the downstream device is the keyboard"
    );
    assert_eq!(descriptor.vendor_id, 0x046D);
    assert!(
        device.device_present(),
        "the freshly-attached keyboard is now live"
    );

    // Keystrokes flow over the freshly-enumerated slot.
    arm_report_request(&mut device);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
    device.host_mut().process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    let len = device
        .next_report(&mut buf)
        .expect("a report drains")
        .expect("a report is available after the cold-boot attach");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04, "the 'a' keycode reaches the report buffer");
}

#[test]
fn addressing_a_downstream_keyboard_marks_the_parent_hub_as_a_hub() {
    // The metal regression: the keyboard behind the onboard hub was
    // addressed (`4128`) but never delivered a report, because the hub's
    // slot context was left with the Hub bit clear, so the controller
    // never scheduled the full-speed keyboard's split transactions. The
    // fix issues a Configure Endpoint over the hub's slot that sets the
    // Hub bit, Number of Ports, and TT Think Time before addressing the
    // device behind it. The mock requires the Hub bit on that command and
    // delivers no downstream interrupt report until it is set, so this
    // test fails before the fix (no report) and passes after.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    // A full-speed downstream keyboard (the metal case): its interrupt
    // transfers must be split through the hub's TT.
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);

    device.enumerate_hid(1).expect("the hub enumerates");
    device.power_hub_port(4).expect("power the downstream port");
    device.reset_hub_port(4).expect("reset the downstream port");
    let status = device.hub_port_status(4).expect("status after reset");
    device
        .enumerate_downstream_hid(4, hub_port_speed(status))
        .expect("the keyboard behind the hub is addressed");

    // The parent hub was marked a hub with its real port count, the
    // precondition for the controller to route/split to the keyboard.
    assert!(
        device.host_mut().hub_marked_as_hub,
        "the hub's slot context gets the Hub bit before the downstream device is addressed"
    );
    assert_eq!(
        device.host_mut().hub_ctx_num_ports,
        4,
        "the hub's downstream port count reaches the slot context"
    );

    // With the hub marked, a requested report now drains — keystrokes flow.
    arm_report_request(&mut device);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,]);
    device.host_mut().process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    let len = device
        .next_report(&mut buf)
        .expect("a report drains")
        .expect("a report is available once the hub is marked");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04);
}

#[test]
fn the_downstream_interrupt_endpoint_carries_a_nonzero_max_esit_payload() {
    // The metal regression: the full-speed keyboard behind the onboard
    // hub was addressed (`4128`) and the hub marked, yet typing produced
    // nothing and the poll-loop heartbeat (`4131`) climbed with
    // `events=0` — the controller serviced the interrupt endpoint never.
    // Root cause: the endpoint context left Max ESIT Payload zero
    // (§6.2.3.8 dword 4 bits 16:31), so the periodic scheduler reserved
    // no bandwidth for the split transactions (§4.14.2). The fix
    // programs Max ESIT Payload = the max packet size for a periodic
    // endpoint. The mock now delivers no interrupt report while it is
    // zero, so this test fails before the fix (no report drains, and the
    // payload assertion fails) and passes after.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0; // full-speed downstream device
    let mut device = started_device(mock, &mem);

    device.enumerate_hid(1).expect("the hub enumerates");
    device.power_hub_port(4).expect("power the downstream port");
    device.reset_hub_port(4).expect("reset the downstream port");
    let status = device.hub_port_status(4).expect("status after reset");
    device
        .enumerate_downstream_hid(4, hub_port_speed(status))
        .expect("the keyboard behind the hub is addressed");

    assert_ne!(
        device.host_mut().int_max_esit,
        0,
        "the interrupt-IN endpoint context carries a non-zero Max ESIT \
         Payload so the periodic scheduler reserves bandwidth for it"
    );

    // And, with bandwidth reserved, a requested report actually drains.
    arm_report_request(&mut device);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,]);
    device.host_mut().process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    let len = device
        .next_report(&mut buf)
        .expect("a report drains")
        .expect("a report is available once the endpoint has bandwidth");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04);
}

#[test]
fn downstream_keyboard_is_serviced_on_its_descriptor_reported_endpoint() {
    // The metal regression after every prior fix: the keyboard behind
    // the onboard hub was addressed (`4128`) and the hub marked, the
    // interrupt endpoint carried a non-zero Max ESIT Payload, yet typing
    // produced nothing and the poll loop spun with `events=0`. Root
    // cause: the driver hard-coded the interrupt endpoint as endpoint 1
    // (DCI 3); a keyboard whose interrupt-IN endpoint is elsewhere left
    // the controller polling — and the doorbell ringing — the wrong DCI,
    // so it scheduled the real endpoint never.
    //
    // This keyboard reports its interrupt-IN endpoint as **endpoint 2**
    // (DCI 5). The fix reads the endpoint descriptor and configures,
    // doorbells, and drains DCI 5. The mock derives the configured DCI
    // from the Configure Endpoint add flags and posts interrupt events
    // with it, so before the fix the report would arrive on DCI 3 (which
    // the driver no longer expects) — the report does not drain — and
    // after the fix it drains on DCI 5.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0; // full-speed downstream device
    mock.keyboard_config = &MOCK_CONFIG_DESCRIPTOR_EP2;
    let mut device = started_device(mock, &mem);

    device.enumerate_hid(1).expect("the hub enumerates");
    device.power_hub_port(4).expect("power the downstream port");
    device.reset_hub_port(4).expect("reset the downstream port");
    let status = device.hub_port_status(4).expect("status after reset");
    let keyboard = device
        .enumerate_downstream_hid(4, hub_port_speed(status))
        .expect("the keyboard behind the hub is addressed on its real endpoint");
    assert!(!keyboard.is_hub());

    // The Configure Endpoint named DCI 5 (endpoint 2 IN), read from the
    // endpoint descriptor — not the assumed DCI 3.
    assert_eq!(
        device.host_mut().int_dci,
        5,
        "the interrupt endpoint is configured at the descriptor-reported DCI 5"
    );

    // A requested report drains: the controller services DCI 5 and the
    // driver accepts the Transfer Event for that endpoint id.
    arm_report_request(&mut device);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,]);
    device.host_mut().process_int_ring();
    let mut buf = [0u8; REPORT_LEN];
    let len = device
        .next_report(&mut buf)
        .expect("a report drains")
        .expect("a report is available on the endpoint the keyboard actually uses");
    assert_eq!(len, REPORT_LEN);
    assert_eq!(buf[2], 0x04, "the 'a' keycode reaches the report buffer");
}

#[test]
fn enumerate_downstream_hid_omits_the_tt_for_a_high_speed_device() {
    // A high-speed device behind a high-speed hub needs no transaction
    // translator: its slot context's TT fields stay zero (xHCI §6.2.2).
    // The mock faults Address Device if a TT is programmed for a
    // high-speed device, so success proves the driver omits it.
    let mem = shared_mem();
    // `with_hub` defaults the downstream device to high speed.
    let mock = MockXhci::with_hub(&mem, 4, 3);
    let mut device = started_device(mock, &mem);

    device.enumerate_hid(1).expect("the hub enumerates");
    device.power_hub_port(3).expect("power the downstream port");
    device.reset_hub_port(3).expect("reset the downstream port");
    let status = device.hub_port_status(3).expect("status after reset");
    assert_eq!(hub_port_speed(status), 3, "high-speed downstream device");

    let keyboard = device
        .enumerate_downstream_hid(3, hub_port_speed(status))
        .expect("a high-speed downstream HID device is addressed without a TT");
    assert!(!keyboard.is_hub());
    assert_eq!(device.host_mut().downstream_route_port, 3);
}

#[test]
fn enumerate_downstream_hid_before_a_hub_is_addressed_fails_closed() {
    // Addressing a downstream device requires a hub already addressed on
    // the active slot (its slot is the route's root and its TT hub).
    // Without one the call fails closed rather than addressing a device
    // at a guessed topology.
    let mem = shared_mem();
    let mock = MockXhci::with_hub(&mem, 4, 4);
    let mut device = started_device(mock, &mem);
    assert_eq!(
        device.enumerate_downstream_hid(4, 1),
        Err(DriverError::DeviceFault),
    );
}

#[test]
fn hub_num_ports_fails_closed_on_a_forged_descriptor() {
    // A hub descriptor with the wrong bDescriptorType is forged/corrupt
    // and rejected fail-closed.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 2);
    mock.forge_hub_descriptor = true;
    let mut device = started_device(mock, &mem);
    device.enumerate_hid(1).expect("the hub enumerates");
    assert_eq!(device.hub_num_ports(), Err(DriverError::BadMagic));
}

#[test]
fn faulting_hub_port_status_records_the_completion_code() {
    // The metal capture reached `4127` for every downstream port but
    // each `wstatus` read as the all-ones sentinel — the per-port class
    // `GET_STATUS` faulted while the hub-descriptor read and Port-Power
    // writes succeeded. The bring-up diagnostic surfaces the raw xHCI
    // completion code so a metal capture can tell *why*; this pins that a faulting `GET_STATUS` fails closed and
    // leaves `last_completion_code()` at the failing code rather than a
    // stale success.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 2);
    mock.fault_hub_port_status = true;
    let mut device = started_device(mock, &mem);
    device.enumerate_hid(1).expect("the hub enumerates");

    assert_eq!(
        device.hub_port_status(2),
        Err(DriverError::DeviceFault),
        "a STALLed GET_STATUS fails closed"
    );
    assert_eq!(
        device.last_completion_code(),
        CompletionCode::StallError.as_u8(),
        "the failing completion code is preserved for the diagnostic"
    );
}

#[test]
fn faulting_hub_port_status_records_an_undecodable_completion_code() {
    // The metal capture reported `completion_hex=0` for every per-port
    // `GET_STATUS` — but the fast (logging-cadence) failure means an
    // event *did* arrive; `0` is the diagnostic mislabelling a
    // real-but-rejected code as a timeout. `await_event_for` previously
    // returned before the caller recorded the code whenever the event
    // carried a completion code this driver does not model (its
    // fail-closed `completion_code()` decode), leaving
    // `last_completion_code()` at the `0` "no event" sentinel. The fix
    // records the raw code as the event is observed, so a reserved /
    // controller-specific code (here xHCI `7`, Resource Error) survives
    // for the metal capture. This fails before the
    // fix (code lost to `0`) and passes after.
    const RESOURCE_ERROR: u8 = 7;
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 2);
    mock.fault_hub_port_status_raw = RESOURCE_ERROR;
    let mut device = started_device(mock, &mem);
    device.enumerate_hid(1).expect("the hub enumerates");

    assert_eq!(
        device.hub_port_status(2),
        Err(DriverError::OutOfRange),
        "an undecodable GET_STATUS completion fails closed on the decode"
    );
    assert_eq!(
        device.last_completion_code(),
        RESOURCE_ERROR,
        "the raw, undecodable completion code is preserved for the diagnostic"
    );
}

#[test]
fn faulting_hub_port_status_records_an_unexpected_event_type() {
    // The next metal capture read `completion_hex=0` on two ports with
    // the *fast* failure cadence — i.e. an event arrived but it was not
    // a completion the wait expected. `await_event_for` rejects an event
    // whose TRB-type it does not handle (an asynchronous controller
    // event interleaved with the awaited transfer) via its `_` arm,
    // which records no completion code — so `completion_hex=0` alone
    // cannot tell that from a genuine poll-budget timeout. The reject
    // now records `last_reject_reason()=1` (unexpected type) and the raw
    // type in `last_event_type()`, while `last_completion_code()` stays
    // `0` truthfully (no completion code was carried), distinguishing
    // the two. Fails before the fix (no such
    // accessors / reason lost); passes after.
    let unexpected = TrbType::NoOp.as_u8();
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 2);
    mock.fault_hub_port_status_evtype = unexpected;
    let mut device = started_device(mock, &mem);
    device.enumerate_hid(1).expect("the hub enumerates");

    assert_eq!(
        device.hub_port_status(2),
        Err(DriverError::DeviceFault),
        "an unexpected event type fails the GET_STATUS wait closed"
    );
    assert_eq!(
        device.last_reject_reason(),
        1,
        "the reject reason names an unexpected event type"
    );
    assert_eq!(
        device.last_event_type(),
        unexpected,
        "the rejected event's raw TRB-type is preserved for the diagnostic"
    );
    assert_eq!(
        device.last_completion_code(),
        0,
        "no completion code was carried — truthfully 0, not a timeout label"
    );
}

#[test]
fn reports_flow_through_the_report_source() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");

    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(&mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0, 0, 0x04, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(&mut buf), Ok(Some(REPORT_LEN)));
    assert_eq!(buf, [0, 0, 0x04, 0, 0, 0, 0, 0]);

    // The 3-byte mouse report arrives as a short packet.
    assert_eq!(device.next_report(&mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0x01, 0xFF, 0x02]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(&mut buf), Ok(Some(3)));
    assert_eq!(buf[..3], [0x01, 0xFF, 0x02]);
    assert_eq!(device.next_report(&mut buf), Ok(None));
}

#[test]
fn report_source_rearms_across_the_ring_wrap() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");

    // More reports than the ring's data slots: arming and draining them all
    // proves retire + on-demand arm keep the ring live across the Link-TRB wrap.
    let total = 2 * RING_TRBS;

    let mut buf = [0u8; REPORT_LEN];
    for index in 0..total {
        let marker = u8::try_from(index).expect("small index");
        assert_eq!(device.next_report(&mut buf), Ok(None));
        device
            .host_mut()
            .pending_reports
            .push_back(alloc::vec![marker, 0, 0, 0, 0, 0, 0, 0]);
        device.host_mut().process_int_ring();
        assert_eq!(device.next_report(&mut buf), Ok(Some(REPORT_LEN)));
        assert_eq!(buf[0], marker, "reports arrive in order");
    }
    assert_eq!(device.next_report(&mut buf), Ok(None));
}

#[test]
fn report_source_rearms_after_a_rejected_completion() {
    // A single transfer event the driver rejects per-report (an
    // unexpected completion code) must still leave the interrupt endpoint
    // re-armed, so the *next* report is delivered. Before the re-arm
    // hardening this returned the error *before* retiring/arming the ring,
    // so the endpoint went silent forever and a busy-polling keyboard
    // driver kept reading an empty event ring while the keyboard appeared
    // dead after one keystroke (the on-metal HDMI-console symptom). This
    // fails before the fix (the second report never arrives) and passes
    // after.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");

    // The next report posts a non-Success/ShortPacket completion code the
    // decode rejects; the one after is normal.
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::StallError);
    assert_eq!(device.next_report(&mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();

    // The rejected report surfaces a per-report fault…
    assert_eq!(device.next_report(&mut buf), Err(DriverError::DeviceFault));
    // …but the ring was retired, so the next class request can arm a fresh
    // transfer and the following good report still arrives rather than the
    // keyboard going permanently silent.
    assert_eq!(device.next_report(&mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0, 0, 0x05, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(&mut buf), Ok(Some(REPORT_LEN)));
    assert_eq!(buf, [0, 0, 0x05, 0, 0, 0, 0, 0]);
    assert_eq!(device.next_report(&mut buf), Ok(None));
}

#[test]
fn rejected_report_records_its_completion_code_surviving_a_later_control_transfer() {
    // When a downstream keyboard is unplugged, on metal the disconnect first
    // surfaces as the device's interrupt-IN transfer faulting. The HCD then
    // issues a hub GET_PORT_STATUS control transfer to confirm — which resets
    // the shared per-transfer event diagnostics. The controller's verdict on
    // the keyboard's *own* endpoint (a transient transaction error vs. a
    // device-gone code) is the datum that decides the correct teardown, so it
    // must be captured at the report fault and survive that confirmation
    // control transfer. This asserts the dedicated `last_report_fault_code`
    // records the rejected code and is not clobbered by a subsequent control
    // transfer (it fails before that field existed, when the code was lost).
    use crate::transport::UrbEngine;
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");
    assert_eq!(
        device.last_report_fault_code(),
        0,
        "no report has faulted yet"
    );

    // The next interrupt-IN report posts a completion code the decode rejects
    // (the unplug-style fault).
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::UsbTransactionError);
    assert_eq!(device.next_report(&mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(&mut buf), Err(DriverError::DeviceFault));
    assert_eq!(
        device.last_report_fault_code(),
        CompletionCode::UsbTransactionError.as_u8(),
        "the rejected report's completion code is captured"
    );

    // A subsequent control transfer (standing in for the hub disconnect
    // confirmation the HCD issues next) resets the shared event diagnostics
    // but must leave the report fault code intact.
    let mut descriptor = [0u8; 18];
    let get_device_descriptor = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00];
    device
        .control_in(get_device_descriptor, &mut descriptor)
        .expect("the device-descriptor control transfer completes");
    assert_eq!(
        device.last_completion_code(),
        CompletionCode::Success.as_u8(),
        "the control transfer reset the shared diagnostics to its own result"
    );
    assert_eq!(
        device.last_report_fault_code(),
        CompletionCode::UsbTransactionError.as_u8(),
        "the report fault code survives a later control transfer"
    );
}

#[test]
fn next_report_before_enumeration_fails_closed() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(&mut buf), Err(DriverError::DeviceFault));
}

#[test]
fn enable_interrupter_arms_iman_imod_and_usbcmd_inte() {
    // A driver that services the keyboard interrupt-driven enables the
    // controller interrupter once: this sets the per-interrupter Interrupt
    // Enable, disables moderation (lowest completion latency), clears any
    // stale Interrupt Pending the firmware left, and sets the global
    // `USBCMD.INTE` so a posted event asserts the device's interrupt.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");
    // Seed a stale Interrupt Pending the firmware hand-off could leave.
    device.host_mut().iman = regs::IMAN_IP;

    device.enable_interrupter().expect("enable interrupter");

    let host = device.host_mut();
    assert_eq!(
        host.iman & regs::IMAN_IE,
        regs::IMAN_IE,
        "interrupter Interrupt Enable is set"
    );
    assert_eq!(
        host.iman & regs::IMAN_IP,
        0,
        "the stale Interrupt Pending was cleared"
    );
    assert_eq!(
        host.imod, 0,
        "interrupt moderation disabled (lowest latency)"
    );
    assert_eq!(
        host.usbcmd & regs::USBCMD_INTE,
        regs::USBCMD_INTE,
        "global Interrupter Enable is set"
    );
}

#[test]
fn enable_interrupter_clears_stale_global_status_before_arming() {
    // Port-change and event latches can be left visible by the discovery /
    // enumeration path. Clear them before enabling xHCI interrupts so the
    // first real report completion produces a fresh controller interrupt.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");
    device.host_mut().hse_latched = true;
    device.host_mut().eint_latched = true;
    device.host_mut().pcd_latched = true;
    device.host_mut().status_write_needs_read_flush = true;

    device.enable_interrupter().expect("enable interrupter");

    let host = device.host_mut();
    assert_eq!(
        host.read32(MockXhci::op(regs::USBSTS)).unwrap()
            & (regs::USBSTS_HSE | regs::USBSTS_EINT | regs::USBSTS_PCD),
        0,
        "stale global status was cleared and flushed before arming"
    );
    assert_eq!(
        host.iman & regs::IMAN_IE,
        regs::IMAN_IE,
        "interrupter is still armed after stale status cleanup"
    );
    assert_eq!(
        host.usbcmd & regs::USBCMD_INTE,
        regs::USBCMD_INTE,
        "global interrupt enable is still set after stale status cleanup"
    );
}

#[test]
fn interrupter_snapshot_reads_status_without_acknowledging() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");
    device.enable_interrupter().expect("enable interrupter");
    device.host_mut().hse_latched = true;
    device.host_mut().eint_latched = true;
    device.host_mut().iman |= regs::IMAN_IP;

    let snapshot = device
        .interrupter_snapshot()
        .expect("snapshot reads interrupt registers");

    assert_eq!(snapshot.usbsts & regs::USBSTS_HSE, regs::USBSTS_HSE);
    assert_eq!(snapshot.usbsts & regs::USBSTS_EINT, regs::USBSTS_EINT);
    assert_eq!(snapshot.iman & regs::IMAN_IE, regs::IMAN_IE);
    assert_eq!(snapshot.iman & regs::IMAN_IP, regs::IMAN_IP);
    assert_eq!(snapshot.erdp_low, device.host_mut().erdp[0]);
    assert_eq!(snapshot.erdp_high, device.host_mut().erdp[1]);
    assert_eq!(
        device.host_mut().iman & regs::IMAN_IP,
        regs::IMAN_IP,
        "snapshot read must not acknowledge Interrupt Pending"
    );
}

#[test]
fn acknowledge_interrupt_clears_global_and_interrupter_pending_and_keeps_enable() {
    // Servicing a delivered interrupt clears `USBSTS.EINT` and `IMAN.IP`
    // before draining the event ring, keeping Interrupt Enable set so the
    // interrupter stays armed for the next completion.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");
    device.enable_interrupter().expect("enable interrupter");
    // The controller posts an event and sets both interrupt-status latches.
    device.host_mut().eint_latched = true;
    device.host_mut().iman |= regs::IMAN_IP;

    device
        .acknowledge_interrupt()
        .expect("acknowledge interrupt");

    let host = device.host_mut();
    assert_eq!(
        host.read32(MockXhci::op(regs::USBSTS)).unwrap() & regs::USBSTS_EINT,
        0,
        "global Event Interrupt status was cleared"
    );
    assert_eq!(
        host.iman & regs::IMAN_IP,
        0,
        "Interrupt Pending was cleared"
    );
    assert_eq!(
        host.iman & regs::IMAN_IE,
        regs::IMAN_IE,
        "Interrupt Enable stays set after the acknowledge"
    );
}

#[test]
fn forged_report_residual_fails_closed() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");
    let mut buf = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(&mut buf), Ok(None));
    device.host_mut().forge_report_residual = true;
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0, 0, 0x04, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(&mut buf), Err(DriverError::DeviceFault));
}

#[test]
fn boot_keyboard_decodes_over_the_xhci_transfer_ring() {
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");
    let mut arm = [0u8; REPORT_LEN];
    assert_eq!(device.next_report(&mut arm), Ok(None));
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
fn interface_info_decodes_and_fails_closed() {
    // The boot-keyboard fixture: config value 1, interface 0, class
    // `0x03_01_01`.
    let info = InterfaceInfo::decode(&MOCK_CONFIG_DESCRIPTOR).expect("fixture decodes");
    assert_eq!(info.configuration_value, 1);
    assert_eq!(info.interface_number, 0);
    assert_eq!(info.class24, 0x03_01_01);

    // Too short to hold the configuration header.
    assert_eq!(
        InterfaceInfo::decode(&MOCK_CONFIG_DESCRIPTOR[..8]),
        Err(DriverError::BadMagic)
    );
    // Leading descriptor is not a configuration descriptor.
    let mut wrong_type = MOCK_CONFIG_DESCRIPTOR;
    wrong_type[1] = 0x01;
    assert_eq!(
        InterfaceInfo::decode(&wrong_type),
        Err(DriverError::BadMagic)
    );
    // An interface descriptor claiming a length that runs off the end.
    let mut runaway = MOCK_CONFIG_DESCRIPTOR;
    runaway[9] = 0xFF;
    assert_eq!(InterfaceInfo::decode(&runaway), Err(DriverError::BadMagic));
    // A configuration with no interface descriptor at all (only the
    // 9-byte header).
    assert_eq!(
        InterfaceInfo::decode(&MOCK_CONFIG_DESCRIPTOR[..9]),
        Err(DriverError::BadMagic)
    );
    // A second interface class is honoured (boot mouse `0x03_01_02`).
    let mut mouse = MOCK_CONFIG_DESCRIPTOR;
    mouse[16] = 0x02;
    assert_eq!(
        InterfaceInfo::decode(&mouse)
            .expect("mouse decodes")
            .class24,
        0x03_01_02
    );
}

#[test]
fn describe_device_emits_the_hid_child_node() {
    use rustos_abi::{HwDeviceClass, HwMatchKey};
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    device.enumerate_hid(1).expect("enumeration succeeds");

    // The emitted child node carries the device's vid:pid and the
    // *interface* class read from the configuration descriptor
    // (`0x03_01_01`), parented at the controller node and assigned the
    // tree owner's id.
    let node = device.describe_device(7, 9).expect("identity captured");
    assert_eq!(node.id(), 9);
    assert_eq!(node.parent(), 7);
    assert_eq!(node.class(), Some(HwDeviceClass::Input));
    assert_eq!(node.match_keys().len(), 1);
    let emitted = node.match_keys()[0];
    assert_eq!(emitted, HwMatchKey::usb(0x046D, 0xC077, 0x03_01_01));

    // A HID boot-keyboard class bind key (HID class `0x03_01_01`, the key the
    // `usb_kbd` class driver carries) resolves against the emitted node by
    // class (vendor/product wildcard), exactly as `devmgr` will. Constructed
    // inline so this protocol crate does not depend on a concrete driver.
    let keyboard_key = HwMatchKey::usb(0, 0, 0x03_01_01);
    assert!(keyboard_key.matches(&emitted));
    // A boot-mouse bind key (HID class `0x03_01_02`) must not bind a keyboard
    // interface.
    let mouse_key = HwMatchKey::usb(0, 0, 0x03_01_02);
    assert!(!mouse_key.matches(&emitted));
}

#[test]
fn describe_device_before_enumeration_fails_closed() {
    let mem = shared_mem();
    let device = started_device(MockXhci::with_device(&mem), &mem);
    // No device enumerated yet: the identity is absent, so the bus
    // refuses to fabricate a node.
    assert_eq!(
        device.describe_device(7, 9).err(),
        Some(DriverError::NotFound)
    );
}

/// `C_PORT_CONNECTION` (USB 2.0 §11.24.2.7.2.1) — the connect-status-change
/// bit a hub latches in `wPortChange`, which the watch reads and clears.
const PORT_CHANGE_CONNECTION: u16 = 1 << 0;

#[test]
fn hub_watch_arms_after_enumerating_through_a_hub() {
    // Reaching the keyboard through the onboard hub arms the hub's
    // status-change endpoint, so a later downstream connect/disconnect is
    // delivered event-driven rather than polled.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up_keyboard(&delay)
        .expect("the keyboard behind the hub is reached");
    assert!(
        device.hub_watch_active(),
        "the hub status-change watch is armed once a hub is descended"
    );
    // With no change pending, servicing the watch is a no-op (it parks on the
    // controller interrupt, never polling).
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::None));
}

#[test]
fn enumeration_drains_every_port_change_latch_so_the_hub_watch_stays_quiet() {
    // Real hubs latch a Reset-change (`wPortChange` bit 4) when a downstream
    // port is reset during enumeration, alongside the connect change. The hub
    // keeps its status-change endpoint asserting a report for that port until
    // *every* latched change is cleared. Clearing only the connect change
    // leaves the reset change latched, so the freshly-armed watch fires
    // immediately and forever on a stale change — drowning/faulting the
    // keyboard's reports. This is the metal regression: enumeration must drain
    // the whole change set so the watch goes quiet until a real hot-plug.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    mock.hub_downstream_change = PORT_CHANGE_CONNECTION;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up_keyboard(&delay)
        .expect("the keyboard behind the hub is reached");

    // Enumeration reset the downstream port (latching the Reset-change) and
    // must have drained both that and the connect change, so nothing remains
    // for the status-change endpoint to report.
    assert_eq!(
        device.host_mut().hub_downstream_change,
        0,
        "enumeration must clear every port-change latch, not just connect"
    );

    // A status-change report with no genuine change pending is a no-op: the
    // watch fabricates neither a connect nor a disconnect, and leaves the port
    // clear (no re-arm storm).
    device.host_mut().post_hub_status_change(&[1 << 4]);
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::None));
    assert_eq!(device.host_mut().hub_downstream_change, 0);
    assert!(
        device.device_present(),
        "the keyboard stays enumerated through a spurious status-change report"
    );
}

#[test]
fn hub_watch_retracts_a_disconnected_downstream_device() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up_keyboard(&delay)
        .expect("the keyboard behind the hub is reached");
    assert_eq!(device.slot(), 2, "the keyboard occupies the second slot");

    // Unplug the keyboard: its hub port now reads disconnected with the
    // connect-status change latched, and the hub posts a status-change report
    // naming downstream port 4 (bit 4 of the change bitmap).
    device.host_mut().hub_downstream_status = 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);

    assert_eq!(
        device.next_hub_change(&delay),
        Ok(HubEvent::Detached),
        "the disconnected downstream device is detected"
    );
    assert!(!device.device_present(), "its device slot was freed");
    assert!(
        device.hub_watch_active(),
        "the controller and its hub watch stay up after a detach"
    );
}

#[test]
fn faulted_downstream_report_can_confirm_and_detach_a_gone_device() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up_keyboard(&delay)
        .expect("the keyboard behind the hub is reached");
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::StallError);
    assert_eq!(device.next_report(&mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();

    device.host_mut().hub_downstream_status = 0;
    assert_eq!(device.next_report(&mut buf), Err(DriverError::DeviceFault));
    assert_eq!(device.detach_if_watched_device_gone(), Ok(true));
    assert!(
        !device.device_present(),
        "the vanished device slot was freed"
    );
    assert!(
        device.hub_watch_active(),
        "the hub watch remains armed for a later reattach"
    );
}

#[test]
fn fault_driven_detach_rearms_a_stashed_hub_change_for_reattach() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up_keyboard(&delay)
        .expect("the keyboard behind the hub is reached");
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::StallError);
    assert_eq!(device.next_report(&mut buf), Ok(None));
    device.host_mut().hub_downstream_status = 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();

    assert_eq!(device.next_report(&mut buf), Err(DriverError::DeviceFault));
    assert_eq!(device.detach_if_watched_device_gone(), Ok(true));

    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::None));
    device.host_mut().hub_downstream_status = 1 << 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(descriptor)) => {
            assert!(!descriptor.is_hub());
            assert_eq!(descriptor.vendor_id, 0x046D);
            assert_eq!(descriptor.product_id, 0xC077);
        }
        other => panic!("expected a fresh attach after re-arming the hub watch, got {other:?}"),
    }
}

#[test]
fn trailing_freed_slot_transfer_event_is_drained_not_faulted() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up_keyboard(&delay)
        .expect("the keyboard behind the hub is reached");
    let freed_slot = device.raw_device_slot();
    assert!(freed_slot != 0, "the keyboard enumerated on a real slot");

    // The unplug faults the device's interrupt-IN transfer; the fault path
    // confirms the downstream port is gone and frees the device slot.
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::StallError);
    assert_eq!(device.next_report(&mut buf), Ok(None));
    device.host_mut().hub_downstream_status = 0;
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(&mut buf), Err(DriverError::DeviceFault));
    assert_eq!(device.detach_if_watched_device_gone(), Ok(true));

    // The controller now posts a *trailing* transfer completion still addressed
    // to the just-freed device slot — ahead of the hub's disconnect
    // status-change report on the shared event ring. Before the fix this
    // matched no live endpoint and faulted the hub watch.
    device.host_mut().post_transfer_event_for_slot(
        0x4242,
        CompletionCode::StallError,
        3,
        0,
        freed_slot,
    );
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);

    // The stale event is drained, not faulted: the hub change is serviced
    // quietly (the device is already gone) and the watch stays armed.
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::None));
    assert!(
        device.stale_freed_event_count() >= 1,
        "the trailing freed-slot completion was drained, not faulted"
    );
    assert!(
        device.hub_watch_active(),
        "the hub watch survived the stale event and is armed for a reconnect"
    );

    // A genuine reconnect still enumerates a brand-new device on a fresh slot.
    device.host_mut().hub_downstream_status = 1 << 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(descriptor)) => {
            assert!(!descriptor.is_hub());
            assert_eq!(descriptor.vendor_id, 0x046D);
            assert_eq!(descriptor.product_id, 0xC077);
        }
        other => panic!("expected a fresh attach after draining the stale event, got {other:?}"),
    }
    // Once the fresh device owns its slot the freed-slot tolerance is cleared.
    assert!(device.device_present());
}

#[test]
fn fault_driven_detach_leaves_unposted_hub_latch_for_rearm() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up_keyboard(&delay)
        .expect("the keyboard behind the hub is reached");
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::StallError);
    assert_eq!(device.next_report(&mut buf), Ok(None));
    device.host_mut().hub_downstream_status = 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();

    assert_eq!(device.next_report(&mut buf), Err(DriverError::DeviceFault));
    assert_eq!(device.detach_if_watched_device_gone(), Ok(true));
    assert_eq!(
        device.host_mut().hub_downstream_change,
        PORT_CHANGE_CONNECTION,
        "the hub latch stays set until the status endpoint reports it"
    );

    device.host_mut().post_hub_status_change(&[1 << 4]);
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::None));
    assert_eq!(device.host_mut().hub_downstream_change, 0);

    device.host_mut().hub_downstream_status = 1 << 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(descriptor)) => {
            assert!(!descriptor.is_hub());
            assert_eq!(descriptor.vendor_id, 0x046D);
            assert_eq!(descriptor.product_id, 0xC077);
        }
        other => panic!("expected a fresh attach after the delayed hub re-arm, got {other:?}"),
    }
}

#[test]
fn live_downstream_report_fault_is_not_misclassified_as_detach() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up_keyboard(&delay)
        .expect("the keyboard behind the hub is reached");
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::StallError);
    assert_eq!(device.next_report(&mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();

    assert_eq!(device.next_report(&mut buf), Err(DriverError::DeviceFault));
    assert_eq!(device.detach_if_watched_device_gone(), Ok(false));
    assert!(
        device.device_present(),
        "a live device's transfer fault remains a report fault"
    );
}

#[test]
fn split_transaction_fault_detaches_without_a_hub_status_confirmation() {
    // The metal case: a low/full-speed keyboard hangs off a hub that stays
    // plugged in, so on unplug the hub's downstream port keeps reading
    // connected and a hub `GET_PORT_STATUS` confirmation is unreliable (it
    // times out). The disconnect surfaces *only* as the keyboard's own
    // interrupt-IN transfer faulting with a Split Transaction Error (the hub's
    // transaction translator can no longer reach the gone device). That code is
    // conclusive on its own, so the device slot must be freed directly —
    // without depending on the hub confirmation, which here would (wrongly)
    // report the port still connected and leave the device wedged forever.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up_keyboard(&delay)
        .expect("the keyboard behind the hub is reached");

    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::SplitTransactionError);
    assert_eq!(device.next_report(&mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(&mut buf), Err(DriverError::DeviceFault));
    assert_eq!(
        device.last_report_fault_code(),
        CompletionCode::SplitTransactionError.as_u8(),
        "the keyboard endpoint's device-gone code is captured"
    );

    // The hub's downstream port is deliberately left reading connected: the fix
    // must NOT depend on the hub confirmation. Before the fix this returned
    // Ok(false) (hub says connected) and the device was never freed.
    assert_eq!(device.detach_if_watched_device_gone(), Ok(true));
    assert!(!device.device_present(), "the gone device's slot was freed");
    assert!(
        device.hub_watch_active(),
        "the hub watch stays armed for the re-plug"
    );
    assert_eq!(
        device.last_report_fault_code(),
        0,
        "the acted-on fault code is cleared so a re-plug is not re-detached"
    );

    // Re-plug: the hub posts a connect change and the device re-enumerates on a
    // fresh slot.
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(descriptor)) => {
            assert!(!descriptor.is_hub());
            assert_eq!(descriptor.vendor_id, 0x046D);
            assert_eq!(descriptor.product_id, 0xC077);
        }
        other => {
            panic!("expected a fresh attach after the split-transaction detach, got {other:?}")
        }
    }
    assert!(
        device.device_present(),
        "the re-plugged keyboard is live again"
    );
}

#[test]
fn split_transaction_detach_frees_the_slot_even_when_disable_is_never_confirmed() {
    // The decisive metal case (matching the captured log): the keyboard's
    // interrupt-IN endpoint faults with a Split Transaction Error AND the
    // controller never lets the Disable Slot command complete — the gone
    // device's hub cannot acknowledge it, so the teardown's command wait times
    // out. The teardown must still free the slot *locally* (best-effort), or
    // `device_slot` stays set, `process_hub_change` ignores the re-plug connect
    // (it enumerates only when no device is tracked), and the keyboard is never
    // re-detected — exactly the "no log on re-plug" symptom.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up_keyboard(&delay)
        .expect("the keyboard behind the hub is reached");

    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::SplitTransactionError);
    assert_eq!(device.next_report(&mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(&mut buf), Err(DriverError::DeviceFault));

    // The controller will NOT acknowledge the Disable Slot — model the metal
    // controller that never posts the completion the teardown waits for.
    device.host_mut().suppress_disable_completion = true;

    // The slot is still freed locally despite the unconfirmable Disable Slot.
    assert_eq!(device.detach_if_watched_device_gone(), Ok(true));
    assert!(
        !device.device_present(),
        "the slot is freed best-effort even without a Disable Slot confirmation"
    );
    assert!(
        !device.slot_disable_confirmed(),
        "the teardown records that the controller never confirmed the Disable Slot"
    );
    assert!(
        device.hub_watch_active(),
        "the hub watch stays armed for the re-plug"
    );

    // Re-plug now re-enumerates (it would not if `device_slot` were still set).
    // The controller acknowledges the re-enumeration's commands again.
    device.host_mut().suppress_disable_completion = false;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(descriptor)) => {
            assert!(!descriptor.is_hub());
            assert_eq!(descriptor.vendor_id, 0x046D);
            assert_eq!(descriptor.product_id, 0xC077);
        }
        other => panic!("expected a fresh attach after an unconfirmed detach, got {other:?}"),
    }
    assert!(
        device.device_present(),
        "the re-plugged keyboard is live again"
    );
}

#[test]
fn a_failed_status_change_service_re_arms_the_watch_so_a_replug_is_still_seen() {
    // The decisive reconnect bug: after a downstream keyboard is torn down on
    // its own device-unreachable fault code, the hub posts a status-change
    // report, but the gone device's transaction translator briefly cannot
    // answer the hub's `GET_PORT_STATUS` (the metal `reject_hex=4` timeout), so
    // servicing that report errors. The status-change endpoint MUST still be
    // re-armed across that error — otherwise it is left with no outstanding
    // transfer, the hub can never post another report, and the later reconnect
    // produces no interrupt at all (the "re-plug not detected" symptom). The
    // engine then never wakes again.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up_keyboard(&delay)
        .expect("the keyboard behind the hub is reached");

    // Unplug: the keyboard's interrupt-IN endpoint faults with a Split
    // Transaction Error and the slot is freed directly (the hub confirmation is
    // unreliable, so the device-unreachable code is conclusive on its own).
    let mut buf = [0u8; REPORT_LEN];
    device.host_mut().fault_one_report_completion = Some(CompletionCode::SplitTransactionError);
    assert_eq!(device.next_report(&mut buf), Ok(None));
    device
        .host_mut()
        .pending_reports
        .push_back(alloc::vec![0xAA, 0, 0, 0, 0, 0, 0, 0]);
    device.host_mut().process_int_ring();
    assert_eq!(device.next_report(&mut buf), Err(DriverError::DeviceFault));
    assert_eq!(device.detach_if_watched_device_gone(), Ok(true));
    assert!(!device.device_present(), "the gone device's slot was freed");
    assert!(device.hub_watch_active());

    // The hub posts a status-change report, but servicing it fails: right
    // after a downstream disconnect the gone device's transaction translator
    // briefly cannot answer the hub's class control transfers (the metal
    // `reject_hex=4`), so reading the hub topology faults. The service
    // therefore returns an error — yet the status-change endpoint MUST still
    // be re-armed across that error, or the watch is left with no outstanding
    // transfer, the hub can never post another report, and the later reconnect
    // produces no interrupt at all (the "re-plug not detected" symptom).
    device.host_mut().forge_hub_descriptor = true;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    assert!(
        device.next_hub_change(&delay).is_err(),
        "the faulting hub control transfer surfaces as an error"
    );
    assert!(
        device.hub_watch_active(),
        "the watch stays active after a failed status-change service"
    );
    assert!(
        !device.device_present(),
        "the failed service enumerated nothing yet"
    );

    // The transient hub fault clears and the keyboard is (re-)plugged. The
    // connect is only delivered if the status-change endpoint was re-armed
    // despite the earlier error — i.e. an interrupt can still reach the engine.
    device.host_mut().forge_hub_descriptor = false;
    device.host_mut().hub_downstream_status = 1 << 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(descriptor)) => {
            assert!(!descriptor.is_hub());
            assert_eq!(descriptor.vendor_id, 0x046D);
            assert_eq!(descriptor.product_id, 0xC077);
        }
        other => {
            panic!("expected a fresh attach after the transient hub fault cleared, got {other:?}")
        }
    }
    assert!(
        device.device_present(),
        "the re-plugged keyboard is live again"
    );
}

#[test]
fn hub_watch_reenumerates_a_reattached_device_on_a_fresh_slot() {
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up_keyboard(&delay)
        .expect("the keyboard behind the hub is reached");

    // Unplug.
    device.host_mut().hub_downstream_status = 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    assert_eq!(device.next_hub_change(&delay), Ok(HubEvent::Detached));

    // Re-plug: the port reads connected again with the change latched. The
    // reconnect is treated as a brand-new device — a fresh slot, no reuse of
    // the old one.
    device.host_mut().hub_downstream_status = 1 << 0;
    device.host_mut().hub_downstream_change = PORT_CHANGE_CONNECTION;
    device.host_mut().post_hub_status_change(&[1 << 4]);
    match device.next_hub_change(&delay) {
        Ok(HubEvent::Attached(descriptor)) => {
            assert!(
                !descriptor.is_hub(),
                "the reattached device is the keyboard"
            );
            assert_eq!(descriptor.vendor_id, 0x046D);
            assert_eq!(descriptor.product_id, 0xC077);
        }
        other => panic!("expected a fresh attach, got {other:?}"),
    }
    assert!(
        device.slot() > 2,
        "a re-attach allocates a brand-new slot, never the freed one"
    );
}

#[test]
fn hub_assembly_unplug_at_root_port_tears_down_and_replug_reenumerates() {
    // On the Pi 4 the keyboard hangs off a hub, and pulling the keyboard out
    // takes that hub with it: the unplug surfaces as the hub's own *root* port
    // losing connection, not as a downstream hub-port change. The hub being
    // gone, it answers neither its status-change interrupt endpoint nor a
    // GET_PORT_STATUS control transfer, so watching only the downstream port
    // never sees the disconnect (the metal symptom: the hub control transfer
    // times out and the re-plug is never enumerated). The engine must notice
    // the root port gone, drop the hub watch and all tracking, and let a
    // re-plug re-enumerate from scratch.
    let mem = shared_mem();
    let mut mock = MockXhci::with_hub(&mem, 4, 4);
    mock.hub_downstream_status = 1 << 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    device
        .bring_up_keyboard(&delay)
        .expect("the keyboard behind the hub is reached");
    assert!(device.hub_watch_active());
    assert!(device.device_present());
    assert_eq!(device.root_port(), 1, "the hub enumerated on root port 1");

    // While the hub is present its root port reads connected, so the check is
    // a no-op and the watch is left intact for the normal status-change path.
    assert_eq!(device.detach_if_hub_root_gone(), Ok(false));
    assert!(device.hub_watch_active());

    // The whole hub assembly is now unplugged: its root port clears the
    // connect bit.
    device.host_mut().portsc[0] = 0;
    assert_eq!(
        device.detach_if_hub_root_gone(),
        Ok(true),
        "the hub assembly vanishing at its root port is detected"
    );
    assert!(
        !device.hub_watch_active(),
        "the hub watch is dropped once the hub itself is gone"
    );
    assert!(
        !device.device_present(),
        "no device is tracked after the hub assembly is removed"
    );

    // A re-plug: the hub assembly reappears on a root port. Treated as a
    // brand-new device, a full reset + re-enumeration brings the keyboard back
    // through the hub.
    device.host_mut().portsc[0] =
        regs::PORTSC_CCS | regs::PORTSC_PED | regs::PORTSC_PP | (3 << regs::PORTSC_SPEED_SHIFT);
    assert!(device.any_root_port_connected());
    match device
        .reset_and_reenumerate(&delay)
        .expect("the controller resets and re-enumerates the reattached hub assembly")
    {
        BringUp::Device(descriptor) => {
            assert!(
                !descriptor.is_hub(),
                "the downstream keyboard is reached again"
            );
            assert_eq!(descriptor.vendor_id, 0x046D);
            assert_eq!(descriptor.product_id, 0xC077);
        }
        BringUp::AwaitingDevice => panic!("the reattached hub+keyboard must enumerate"),
    }
    assert!(
        device.device_present(),
        "the keyboard is live again after the re-plug"
    );
    assert!(
        device.hub_watch_active(),
        "the hub watch is re-armed for the freshly enumerated assembly"
    );
}

#[test]
fn reset_and_reenumerate_brings_up_a_directly_attached_device_as_new() {
    // The recovery path for a directly-attached (no hub) device that
    // reconnected on its root port: a full controller reset + re-enumeration
    // brings it up as a brand-new device.
    let mem = shared_mem();
    let mut device = started_device(MockXhci::with_device(&mem), &mem);
    let delay = TestDelay::default();

    device
        .bring_up_keyboard(&delay)
        .expect("the directly-attached keyboard enumerates");
    assert_eq!(device.slot(), 1);

    let descriptor = match device
        .reset_and_reenumerate(&delay)
        .expect("the controller resets and re-enumerates the device")
    {
        BringUp::Device(descriptor) => descriptor,
        BringUp::AwaitingDevice => panic!("a connected directly-attached device must enumerate"),
    };
    assert!(!descriptor.is_hub());
    assert_eq!(descriptor.vendor_id, 0x046D);
    assert_ne!(device.slot(), 0, "a device is enumerated after the reset");
}

#[test]
fn bring_up_keyboard_comes_up_awaiting_a_connect_when_no_device_is_attached() {
    // The cold-boot path for a directly-attached topology with nothing
    // plugged in: no root-hub port reports a connected device, so bring-up
    // must NOT fail. The controller comes up `AwaitingDevice` (no hub, so the
    // root-port connect watch is used, not a hub status-change watch) and the
    // HCD waits for the first root-port connect rather than failing closed.
    let mem = shared_mem();
    let mut mock = MockXhci::with_device(&mem);
    // No connected device on any root port, and no latent device to assert a
    // connect when the ports are powered.
    mock.portsc[0] = 0;
    let mut device = started_device(mock, &mem);
    let delay = TestDelay::default();

    assert_eq!(
        device.bring_up_keyboard(&delay),
        Ok(BringUp::AwaitingDevice),
        "an empty root hub comes up awaiting a device, not failing"
    );
    assert!(
        !device.hub_watch_active(),
        "no hub is present, so the root-port connect watch is used"
    );
    assert!(!device.device_present(), "no device is live yet");
    assert!(
        !device.any_root_port_connected(),
        "no root port reports a connected device while nothing is attached"
    );

    // A keyboard is now plugged into a root port: the controller resets and
    // enumerates it as a brand-new device.
    device.host_mut().portsc[0] =
        regs::PORTSC_CCS | regs::PORTSC_PED | regs::PORTSC_PP | (3 << regs::PORTSC_SPEED_SHIFT);
    assert!(
        device.any_root_port_connected(),
        "the freshly-attached device is seen on its root port"
    );
    let descriptor = match device
        .reset_and_reenumerate(&delay)
        .expect("the controller resets and enumerates the freshly-attached device")
    {
        BringUp::Device(descriptor) => descriptor,
        BringUp::AwaitingDevice => panic!("the now-connected device must enumerate"),
    };
    assert!(!descriptor.is_hub());
    assert_eq!(descriptor.vendor_id, 0x046D);
    assert!(
        device.device_present(),
        "the keyboard is live after the attach"
    );
}
