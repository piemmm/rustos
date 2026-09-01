//! xHCI device enumeration (xHCI 1.2 §4.3) and the HID interrupt-IN
//! report path.
//!
//! [`UsbDevice`] drives one controller through the full bring-up of a
//! attached devices: port reset, Enable Slot, Address
//! Device, `GET_DESCRIPTOR(device)`, Configure Endpoint,
//! `SET_PROTOCOL(boot)` + `SET_IDLE(indefinite)` for a HID interface (so an
//! idle report endpoint stays quiescent rather than storming the
//! controller), and on-demand interrupt-IN transfer arming, per device. Each
//! served device's [`DeviceEngine`] view implements the `ReportSource` seam
//! from `tairix_abi::driver::input`, so the host-controller driver serves
//! reports straight off the transfer ring over the URB transport to a class
//! driver
//! (`drivers/input/usb_kbd`), whose `tairix_hid` decoders consume them.
//!
//! # Memory seam
//!
//! Every byte the controller shares with the driver lives in a growable,
//! caller-provided bank of DMA chunks behind the [`DmaBank`] trait — on
//! metal the [`crate::SlabBank`] over capability-granted slabs, in host
//! tests a plain shared buffer — so the enumeration state machine is
//! proven host-side against the register-level mock plus an in-memory
//! ring model. The controller's shared structures live in one chunk sized
//! exactly to the silicon's reported geometry; every device and hub gets
//! its own chunk on attach and returns it on detach, so concurrency is
//! bounded by the controller's slots and genuine memory exhaustion, never
//! a compile-time budget. The engine performs every ring read/write
//! through the seam; the ring state machines themselves hold no
//! memory ([`ProducerRing`], [`EventRingCursor`]).

use alloc::vec::Vec;

use tairix_abi::{Delay, DriverError, HwDeviceClass, HwMatchKey, HwNode};

use crate::ring::{EventRingCursor, ProducerRing, PushOutcome};
use crate::trb::{self, CompletionCode, Trb, TrbType};
use crate::{ControllerStatus, DmaProgram, PortStatus, Xhci, XhciHost};

/// Growable device-shared memory the engine and the controller both see:
/// a bank of independently allocated DMA chunks addressed through one
/// virtual offset space.
///
/// The engine sizes nothing up front beyond the controller's own shared
/// structures: each enumerated device's rings and buffers live in a chunk
/// [`Self::grow`]n on attach and [`Self::release`]d on detach, so the
/// number of concurrently served devices is bounded by the controller's
/// silicon (its device slots) and genuine memory exhaustion — never by a
/// compile-time constant.
///
/// Chunk base offsets are never reused: a stale offset kept past its
/// chunk's release maps to no chunk and every access through it fails
/// closed, rather than aliasing a later allocation. Reads and writes are
/// CPU-side and bounds-checked within a single chunk. The implementor
/// owns DMA publication ordering (cache cleaning/invalidation on a
/// non-coherent interconnect).
pub trait DmaBank {
    /// Allocate a fresh zeroed chunk of `len` bytes and return its base
    /// offset in the bank's virtual offset space.
    ///
    /// The chunk's device-visible base is 64-byte aligned at minimum (the
    /// strictest alignment the xHCI context/ring structures need); the
    /// production bank's chunks are page-aligned. The base offset is
    /// 4096-aligned so in-chunk layout arithmetic preserves alignment.
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] on genuine memory exhaustion
    ///   (the DMA pool, or the bank's bookkeeping heap).
    /// * [`DriverError::OutOfRange`] if the chunk would lie beyond the
    ///   device-visible aperture the controller can reach, or `len` is 0.
    fn grow(&mut self, len: usize) -> Result<usize, DriverError>;

    /// Release the chunk whose base offset `grow` returned, returning its
    /// memory to the allocator.
    ///
    /// # Errors
    ///
    /// [`DriverError::NotFound`] if `base` names no live chunk (a double
    /// release or a forged offset — fail closed, never a panic).
    fn release(&mut self, base: usize) -> Result<(), DriverError>;

    /// Device-visible address of virtual offset `offset`.
    ///
    /// # Errors
    ///
    /// [`DriverError::OutOfRange`] if `offset` lies in no live chunk.
    fn phys_of(&self, offset: usize) -> Result<u64, DriverError>;

    /// Copy `buf.len()` bytes at `offset` into `buf`.
    ///
    /// # Errors
    ///
    /// [`DriverError::OutOfRange`] if `[offset, offset + buf.len())` does
    /// not lie wholly within one live chunk.
    fn read(&mut self, offset: usize, buf: &mut [u8]) -> Result<(), DriverError>;

    /// Publish `bytes` at `offset`.
    ///
    /// # Errors
    ///
    /// [`DriverError::OutOfRange`] if `[offset, offset + bytes.len())` does
    /// not lie wholly within one live chunk.
    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<(), DriverError>;
}

/// The parked wait seam the engine's synchronous event waits block through.
///
/// Every wait for a controller completion (`UsbDevice::await_event_for`)
/// and the root-port connect debounce ([`UsbDevice::bring_up`])
/// gives the CPU up between event-ring polls by parking on this seam and is
/// bounded by wall-clock time read from the same seam — never an iteration
/// count, never a spin. On metal the host-controller driver implements it by
/// parking on the controller's bound interrupt line (`irq_wait` with the
/// remaining budget as the deadline), so a completion wakes the task early
/// and a quiet controller costs no CPU; host tests supply a deterministic
/// stand-in whose clock advances on each wait so timeouts terminate.
pub trait EventWait {
    /// A monotonically non-decreasing microsecond timestamp. The epoch is
    /// unspecified; only differences are meaningful.
    fn now_us(&self) -> u64;

    /// Park the calling task until the controller signals a new event or
    /// `budget_us` microseconds elapse, whichever is first. Spurious early
    /// wake-ups are permitted (the caller re-polls and re-checks its
    /// deadline); never returning before the controller's next event *and*
    /// before the budget elapses is not.
    fn wait_us(&self, budget_us: u64);
}

/// Wall-clock budget, in microseconds, for one synchronous completion wait
/// ([`UsbDevice::await_event_for`]): a command or transfer that produces no
/// event within this window is a fault, failed closed. USB 2.0 §9.2.6 gives
/// a device up to 5 s to complete a standard request, the slowest completion
/// the enumeration path legitimately waits on; controller commands complete
/// in microseconds, so any honest completion is orders of magnitude inside
/// this bound. A defence against a dead controller/device, not a capacity.
const AWAIT_EVENT_BUDGET_US: u64 = 5_000_000;

/// Wall-clock window, in microseconds, the boot-time root-port scan
/// ([`UsbDevice::bring_up`]) allows a powered port to
/// report a connect before concluding the root hub is empty: the hub
/// power-on-good ceiling (`bPwrOn2PwrGood` ≤ ~200 ms, USB 2.0 §11.11) plus
/// the 100 ms attach-debounce interval (USB 2.0 §7.1.7.3) with headroom.
/// An empty root hub spends this window parked, then the controller stays
/// up awaiting the first connect event-driven. A protocol settle window,
/// not a scalable capacity.
const CONNECT_WINDOW_US: u64 = 500_000;

/// How many times enumeration re-drives a fresh slot for one device when the
/// controller reports a *transaction* fault (a USB or split transaction
/// error) rather than the device actively responding with an error.
///
/// A CRC/timeout/bad-PID or a hub transaction-translator split failure is
/// exactly what a device that is *present but momentarily disturbed*
/// produces — for instance a keyboard or mouse hammered with input while its
/// control endpoint is still being brought up (the on-metal defect: typing
/// during boot, before USB bring-up, intermittently killed the whole USB
/// controller). Such a fault means the device never received or acknowledged
/// the command, so it stays in its prior (Default) state and a fresh Enable
/// Slot + Address Device cleanly re-drives it. A device that *answers* with an
/// error (STALL, babble) or a forged descriptor is not retried — those
/// re-fail deterministically. This is the enumeration analogue of the report-
/// endpoint transaction-fault recovery, and mirrors the bounded
/// re-initialisation real xHCI stacks perform (Linux `hub_port_init`'s
/// `PORT_INIT_TRIES`). A recovery bound, not a scalable capacity, and never an
/// unbounded retry-until-it-works.
const ENUM_ATTEMPTS: u32 = 4;

/// TRB slots in the command, EP0, and interrupt transfer rings and in
/// the event segment. Protocol working sets for one device, not
/// scalable capacities: the command and EP0 rings only ever hold a
/// single in-flight command or control TD, while the interrupt-IN ring
/// keeps [`INT_ARM_DEPTH`] transfers armed at once (see there).
pub const RING_TRBS: usize = 16;

/// Interrupt-IN transfers the engine keeps armed on a report endpoint at
/// once, so the controller always has a landing TRB for the next report
/// even in the window between one report being delivered to the class
/// driver and that driver submitting its next URB.
///
/// A boot keyboard reports only on a state change, at its polling
/// interval. The class driver reads one report per blocking URB round
/// trip; between the reply and its next submit the endpoint has no URB
/// driving it. If nothing is armed in that window the controller has
/// nowhere to write the interval's report and the device's report is
/// dropped — so a keystroke pressed and released while the class driver
/// (or this host-controller driver) was slow to run, e.g. under heavy CPU
/// load, was silently lost. Keeping several transfers armed means the
/// controller captures each report into its own ring slot regardless of
/// how promptly software re-submits; the xHCI event ring then queues the
/// completions until the class driver drains them one URB at a time.
///
/// Bounded by the ring: the producer ring keeps one slot free to tell
/// full from empty, so at most `RING_TRBS - 2` transfers can be in flight.
/// This depth sits well under that ceiling and comfortably covers the
/// reports a human generates within one class-driver scheduling round
/// trip. A protocol working set, not a scalable capacity.
pub const INT_ARM_DEPTH: usize = 8;

const _: () = assert!(INT_ARM_DEPTH <= RING_TRBS - 2);

/// Minimum TRBs in an xHCI event-ring segment.
pub const EVENT_RING_SEGMENT_MIN_TRBS: usize = 16;

const _: () = assert!(RING_TRBS >= EVENT_RING_SEGMENT_MIN_TRBS);

/// Byte length of one HID **boot-protocol** report — the fixed layout the
/// class drivers and the report FIFO consume (USB HID 1.11 App. B: keyboard
/// 8, mouse 3..=8). A report-protocol report captured off the wire
/// ([`CAPTURE_LEN`]) is rewritten into this layout before it is buffered, so
/// the class-driver-facing report is always this size.
pub const REPORT_LEN: usize = 8;

/// Byte length of one interrupt-IN **capture** buffer slot — the raw report
/// read off the controller before normalisation, ahead of any rewrite into
/// the [`REPORT_LEN`] boot layout.
///
/// A boot-protocol report fits [`REPORT_LEN`], but a **report-protocol**
/// report can be longer than eight bytes: a device that declares a Report ID
/// prefixes every report with an ID byte (a boot keyboard's report is then
/// nine bytes, its final key slot at byte 8), and richer pointers/keyboards
/// pack extra fields. Capturing only eight bytes clipped that final field and
/// the whole report was then dropped — the metal "keyboard registers no
/// keypresses" defect on a Report-ID keyboard behind the hub. The interrupt-IN
/// TRB is armed to this length and the controller short-packets a smaller
/// report, so one slot captures any HID interrupt report in full. A
/// validation bound on device-supplied data (the HID interrupt max-packet
/// ceiling), not a scalable capacity.
pub const CAPTURE_LEN: usize = 64;

const _: () = assert!(CAPTURE_LEN >= REPORT_LEN);

/// Byte length of the hub status-change endpoint report buffer (USB 2.0
/// §11.12.4): the port-change bitmap is one bit per port plus the hub bit,
/// so eight bytes covers up to 63 downstream ports — well beyond any hub
/// this engine descends. A fixed protocol working-set buffer, not a
/// scalable capacity.
const HUB_REPORT_LEN: usize = 8;

/// Byte length of the control-transfer data buffer. Sized to hold a
/// composite device's **whole** configuration descriptor in one read: a
/// wireless keyboard+mouse receiver concatenates two or three interface
/// descriptors with their HID and endpoint descriptors, which overflows a
/// 64-byte read and would truncate the tail interfaces mid-descriptor. A
/// validation bound on device-supplied data, not a scalable capacity: a
/// configuration longer than this is served from its first
/// [`CTRL_DATA_LEN`] bytes only.
const CTRL_DATA_LEN: usize = 512;

/// Byte length of HID Report Descriptor the engine reads and retains: the
/// parser's own validation bound on device-supplied data, never more than one
/// control transfer's data buffer can deliver. One definition, so the read
/// length and the retained length cannot diverge.
const REPORT_DESCRIPTOR_LEN: usize = if tairix_hid::MAX_REPORT_DESCRIPTOR < CTRL_DATA_LEN {
    tairix_hid::MAX_REPORT_DESCRIPTOR
} else {
    CTRL_DATA_LEN
};

/// A retained length is held as a `u16`, so a bound that outgrew one would
/// truncate a descriptor to nothing instead of keeping it.
const _: () = assert!(REPORT_DESCRIPTOR_LEN <= u16::MAX as usize);

/// Interfaces decoded from one configuration descriptor: the servable
/// working set of one device. A composite device (a wireless
/// keyboard+mouse receiver) carries two or three interfaces; further
/// interfaces are ignored rather than trusted. A validation bound on
/// device-supplied data, not a scalable capacity.
pub const MAX_INTERFACES: usize = 4;

/// TRB slots in each bulk transfer ring (one link + [`BULK_SLOTS`] data
/// slots). Sized so several bulk URBs can be outstanding per direction
/// while the whole staging area still fits the fixed controller DMA carve
/// beside the scratchpad pages — a protocol working set, not a scalable
/// capacity (the class driver chunks a large transfer through it at a
/// fixed per-device cost).
pub const BULK_RING_TRBS: usize = 9;

/// Data slots in each bulk transfer ring (the ring's last slot is its
/// permanent Link TRB). Each data slot owns one [`BULK_BUF_LEN`] staging
/// buffer, so a completed TRB maps back to its bytes by slot index alone.
pub const BULK_SLOTS: usize = BULK_RING_TRBS - 1;

/// Byte length of one bulk staging buffer — the largest single bulk URB.
/// One controller page: a class driver moves larger transfers as a
/// sequence of URBs through the shared-memory window (the `virtio_blk`
/// chunking precedent), so per-device DMA cost stays fixed.
pub const BULK_BUF_LEN: usize = 4096;

/// The xHCI protocol's ceiling on device slots one controller can expose:
/// `HCSPARAMS1.MaxSlots` is an 8-bit field (xHCI 1.2 §5.3.3), so no
/// controller addresses more than 255 devices concurrently. The engine
/// serves as many devices as the *controller actually reports* — each
/// enumerated device claims a demand-allocated DMA chunk and a table
/// entry, released on detach — so the only concurrency bounds are this
/// protocol ceiling, the controller's own reported slot count, and
/// genuine memory exhaustion (which fails closed as a typed error).
pub const XHCI_MAX_SLOTS: usize = 255;

/// Deepest hub chain a device may sit behind: the xHCI Route String has
/// five four-bit tiers (§8.9.1 / §6.2.2), matching USB 2.0 §4.1.1's
/// five-hub limit. A bound fixed by the protocol, never widened.
pub const MAX_HUB_DEPTH: u8 = 5;

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

/// Endpoint context type field: Bulk OUT (§6.2.3).
const EP_TYPE_BULK_OUT: u32 = 2;

/// Endpoint context type field: Bulk IN (§6.2.3).
const EP_TYPE_BULK_IN: u32 = 6;

/// Device Context Index of the default control endpoint (§4.5.1). Also
/// the [`DeviceState::int_dci`] marker for an interface with no interrupt
/// endpoint (the value never names a real interrupt endpoint).
const DCI_CONTROL: u8 = 1;

/// Hub power-on-good settle, in microseconds, before reading a downstream
/// port's connect status. A USB 2.0 hub reports `bPwrOn2PwrGood` in 2 ms
/// units and is commonly ≤ 100 ms (USB 2.0 §11.11); this fixed budget
/// covers the typical worst case rather than decoding the field. A fixed
/// protocol settle, not a scalable capacity.
const HUB_POWER_ON_GOOD_US: u32 = 100_000;

/// Poll spacing, in microseconds, while awaiting a downstream port's
/// reset completion. A hub drives the reset for 10–20 ms (USB 2.0
/// §11.5.1.5 `TDRST`), so the first poll usually observes it complete;
/// each poll parks the interval on the caller's clock (the hub exposes
/// no interrupt for reset completion while its status-change watch is
/// being serviced, so a bounded `GET_STATUS` re-poll is the protocol's
/// own completion signal). A fixed protocol settle, not a scalable
/// capacity.
const HUB_RESET_POLL_US: u32 = 20_000;

/// Bound on the reset-completion polls: 40 polls of [`HUB_RESET_POLL_US`]
/// give a slow hub 800 ms to enable the port — the budget production
/// stacks allow — after which the attach fails closed. A single fixed
/// 50 ms wait was not enough for slow external hubs, which legitimately
/// take hundreds of milliseconds to complete a downstream reset.
const HUB_RESET_POLLS: u32 = 40;

/// Reset-recovery settle (`TRSTRCY`, USB 2.0 §7.1.7.5), in microseconds,
/// after the port reports reset complete and enabled, before the device
/// behind it is addressed.
const HUB_RESET_SETTLE_US: u32 = 10_000;

/// Bounded attempts for the hub-descriptor read
/// ([`UsbDevice::read_hub_topology`]): production stacks retry this
/// exchange (Linux's hub driver issues it up to three times) because real
/// hubs occasionally answer it wrongly once and honestly on the retry. A
/// fixed protocol retry budget, not a scalable capacity.
const HUB_DESC_ATTEMPTS: u32 = 3;

/// Packs structures into a chunk of the [`DmaBank`]'s offset space at
/// 64-byte alignment — the strictest alignment any xHCI context or ring
/// requires — so every region constructor lays its slices out through the
/// one definition.
struct Packer {
    next: usize,
}

impl Packer {
    /// Start packing at `base` (a [`DmaBank::grow`] chunk base, or `0` to
    /// measure a region's packed length).
    const fn new(base: usize) -> Self {
        Self { next: base }
    }

    /// Claim `len` bytes, returning their offset and advancing to the
    /// next 64-byte boundary.
    const fn take(&mut self, len: usize) -> usize {
        let offset = self.next;
        self.next = (self.next + len).next_multiple_of(64);
        offset
    }
}

/// One tracked hub's status-change watch chunk: the transfer ring and
/// report buffer of its interrupt-IN status-change endpoint (USB 2.0
/// §11.12.3). Every hub the engine keeps addressed — the root-attached
/// hub and each downstream hub — owns exactly one, allocated from the
/// [`DmaBank`] when the hub installs and released when it detaches, so
/// every tier is watched at once. A hub's output context and EP0 ring are
/// not here: the root-attached hub is addressed on the root
/// [`Layout::output_ctx`]/[`Layout::ep0_ring`] before it is known to be a
/// hub, and a downstream hub keeps the [`DeviceRegion`] it was enumerated
/// on ([`HubState::device_region`]).
#[derive(Copy, Clone, Debug, Default)]
struct HubRegion {
    /// The chunk base offset this region was laid out at — the
    /// [`DmaBank::release`] key.
    base: usize,
    /// The hub's interrupt-IN status-change endpoint transfer ring.
    int_ring: usize,
    /// One status-change report buffer for [`Self::int_ring`]: the hub's
    /// port-change bitmap (USB 2.0 §11.12.4, one bit per port plus the
    /// hub bit; [`HUB_REPORT_LEN`] covers up to 63 downstream ports).
    report: usize,
}

impl HubRegion {
    /// Lay the region out inside the chunk granted at `base`.
    const fn at(base: usize) -> Self {
        let mut packer = Packer::new(base);
        Self {
            base,
            int_ring: packer.take(RING_TRBS * trb::TRB_LEN),
            report: packer.take(HUB_REPORT_LEN),
        }
    }

    /// Packed byte length of one hub watch region — the [`DmaBank::grow`]
    /// request that backs [`Self::at`].
    const fn layout_len() -> usize {
        let mut packer = Packer::new(0);
        let _ = packer.take(RING_TRBS * trb::TRB_LEN);
        let _ = packer.take(HUB_REPORT_LEN);
        packer.next
    }
}

/// One served device's demand-allocated chunk: its output device context,
/// default-control-endpoint transfer ring, interrupt-IN transfer ring with
/// its per-slot report buffers, and bulk endpoint rings with their staging
/// buffers. Every enumerated device — the root-attached device, or each
/// device downstream of the addressed hub — owns exactly one, allocated
/// from the [`DmaBank`] when the device attaches and released when it
/// detaches, so all served devices stay live in the DCBAA at once and an
/// idle controller pays for none.
#[derive(Copy, Clone, Debug, Default)]
struct DeviceRegion {
    /// The chunk base offset this region was laid out at — the
    /// [`DmaBank::release`] key.
    base: usize,
    /// The device slot's output device context.
    output_ctx: usize,
    /// The device's default-control-endpoint transfer ring.
    ep0_ring: usize,
    /// Interrupt-IN transfer ring, live only for a HID interface.
    int_ring: usize,
    /// [`RING_TRBS`] report buffers of [`CAPTURE_LEN`] bytes for
    /// [`Self::int_ring`]: slot `n`'s TRB points at buffer `n`, so a
    /// completion maps back to its bytes by slot index. Sized to capture a
    /// whole report-protocol report (not just the [`REPORT_LEN`] boot layout).
    report_bufs: usize,
    /// Bulk-IN transfer ring ([`BULK_RING_TRBS`] slots), live only for a
    /// device whose matched interface carries a bulk-IN endpoint (e.g. a
    /// mass-storage interface).
    bulk_in_ring: usize,
    /// Bulk-OUT transfer ring, as [`Self::bulk_in_ring`].
    bulk_out_ring: usize,
    /// Transfer ring of the interface's **second** bulk-IN endpoint (a UAS
    /// interface's two IN pipes). Its TRBs stage through
    /// [`Self::bulk_in_bufs`]: the URB service holds one URB — and so one
    /// bulk TD — in flight per interface, so the direction's pipes never
    /// race on the buffers.
    bulk_in2_ring: usize,
    /// Second bulk-OUT transfer ring, as [`Self::bulk_in2_ring`].
    bulk_out2_ring: usize,
    /// [`BULK_SLOTS`] staging buffers of [`BULK_BUF_LEN`] bytes for the
    /// bulk-IN rings: slot `n`'s TRB points at buffer `n`, so a completion
    /// maps back to its bytes by slot index.
    bulk_in_bufs: usize,
    /// Staging buffers for the bulk-OUT rings, as [`Self::bulk_in_bufs`].
    bulk_out_bufs: usize,
}

impl DeviceRegion {
    /// Lay the region out inside the chunk granted at `base`, for a
    /// controller with `ctx_size`-byte contexts.
    const fn at(base: usize, ctx_size: usize) -> Self {
        let mut packer = Packer::new(base);
        Self {
            base,
            output_ctx: packer.take(OUTPUT_CONTEXTS * ctx_size),
            ep0_ring: packer.take(RING_TRBS * trb::TRB_LEN),
            int_ring: packer.take(RING_TRBS * trb::TRB_LEN),
            report_bufs: packer.take(RING_TRBS * CAPTURE_LEN),
            bulk_in_ring: packer.take(BULK_RING_TRBS * trb::TRB_LEN),
            bulk_out_ring: packer.take(BULK_RING_TRBS * trb::TRB_LEN),
            bulk_in2_ring: packer.take(BULK_RING_TRBS * trb::TRB_LEN),
            bulk_out2_ring: packer.take(BULK_RING_TRBS * trb::TRB_LEN),
            bulk_in_bufs: packer.take(BULK_SLOTS * BULK_BUF_LEN),
            bulk_out_bufs: packer.take(BULK_SLOTS * BULK_BUF_LEN),
        }
    }

    /// Packed byte length of one device region — the [`DmaBank::grow`]
    /// request that backs [`Self::at`].
    const fn layout_len(ctx_size: usize) -> usize {
        let region = Self::at(0, ctx_size);
        (region.bulk_out_bufs + BULK_SLOTS * BULK_BUF_LEN).next_multiple_of(64)
    }

    /// Region offset of `pipe`'s transfer ring.
    fn bulk_ring_off(&self, pipe: BulkPipe) -> usize {
        match (pipe.direction, pipe.secondary) {
            (BulkDirection::In, false) => self.bulk_in_ring,
            (BulkDirection::In, true) => self.bulk_in2_ring,
            (BulkDirection::Out, false) => self.bulk_out_ring,
            (BulkDirection::Out, true) => self.bulk_out2_ring,
        }
    }

    /// Region offset of `pipe`'s staging buffers (shared per direction).
    fn bulk_bufs_off(&self, pipe: BulkPipe) -> usize {
        match pipe.direction {
            BulkDirection::In => self.bulk_in_bufs,
            BulkDirection::Out => self.bulk_out_bufs,
        }
    }
}

/// Where each **controller-shared** structure lives inside the engine's
/// dedicated shared chunk — the first [`DmaBank::grow`] the engine
/// performs: DCBAA (sized from the controller's reported `MaxSlots`),
/// ERST, command ring, event segment, input context, the root-attached
/// device's output context and EP0 ring, the control data buffer, and the
/// scratchpad. Per-device and per-hub regions are **not** here: each is
/// its own demand-allocated chunk ([`DeviceRegion::at`] /
/// [`HubRegion::at`]), so the shared chunk's size is exactly what the
/// silicon's reported geometry requires.
///
/// All offsets are 64-byte aligned, computed chunk-relative by
/// [`Self::new`] and made absolute in the bank's offset space by
/// [`Self::rebased`].
#[derive(Copy, Clone, Debug)]
struct Layout {
    dcbaa: usize,
    erst: usize,
    command_ring: usize,
    event_segment: usize,
    input_ctx: usize,
    /// The root-attached device slot's output device context: the hub's
    /// when the root device is a hub, otherwise the directly-attached
    /// device's.
    output_ctx: usize,
    /// The root-attached device's default-control-endpoint transfer ring.
    ep0_ring: usize,
    ctrl_data: usize,
    /// Offset of the scratchpad buffer pointer array (xHCI §6.6): one
    /// 64-bit device-visible pointer per scratchpad buffer, the array
    /// `DCBAA[0]` points at. Meaningful only when
    /// [`Self::scratchpad_count`] is non-zero.
    scratchpad_array: usize,
    /// Offset of the first scratchpad buffer page. Each buffer is one
    /// controller page and page-aligned. Meaningful only when
    /// [`Self::scratchpad_count`] is non-zero.
    scratchpad_pages: usize,
    /// Number of scratchpad buffers reserved (`HCSPARAMS2` Max Scratchpad
    /// Buffers; the VL805 needs 31).
    scratchpad_count: usize,
    /// The controller page size each scratchpad buffer occupies.
    page_size: usize,
    ctx_size: usize,
    /// The shared chunk's base offset in the bank ([`Self::rebased`]).
    base: usize,
    /// Packed byte length of the shared chunk — the [`DmaBank::grow`]
    /// request that backs it.
    total: usize,
}

impl Layout {
    /// Compute the shared-structure layout, chunk-relative, for a
    /// controller with `max_slots` device slots, `csz` context size, and
    /// the reported scratchpad geometry. The result's offsets are relative
    /// to a chunk base of `0`; [`Self::rebased`] moves them to the granted
    /// chunk.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if the controller demands scratchpad
    ///   buffers but reports no page size.
    /// * [`DriverError::LengthOutOfRange`] if the scratchpad arithmetic
    ///   overflows (a hostile or broken geometry report).
    fn new(
        max_slots: u8,
        csz: bool,
        scratchpad_count: u32,
        page_size: usize,
    ) -> Result<Self, DriverError> {
        let scratchpad_count = scratchpad_count as usize;
        // A controller that needs scratchpad must report a page size so
        // each buffer can land on a page boundary in the device address
        // space (xHCI §4.20 / §6.6). Fail closed otherwise.
        if scratchpad_count > 0 && page_size == 0 {
            return Err(DriverError::OutOfRange);
        }
        let ctx_size = if csz { 64 } else { 32 };
        let mut packer = Packer::new(0);
        let dcbaa = packer.take((usize::from(max_slots) + 1) * 8);
        let erst = packer.take(16);
        let command_ring = packer.take(RING_TRBS * trb::TRB_LEN);
        let event_segment = packer.take(RING_TRBS * trb::TRB_LEN);
        let input_ctx = packer.take(INPUT_CONTEXTS * ctx_size);
        let output_ctx = packer.take(OUTPUT_CONTEXTS * ctx_size);
        let ep0_ring = packer.take(RING_TRBS * trb::TRB_LEN);
        let ctrl_data = packer.take(CTRL_DATA_LEN);
        let (scratchpad_array, scratchpad_pages) = if scratchpad_count > 0 {
            let array = packer.take(scratchpad_count * 8);
            // The buffer pages must be page-aligned, not merely 64-aligned.
            let pages = packer.next.next_multiple_of(page_size);
            packer.next = pages
                .checked_add(
                    scratchpad_count
                        .checked_mul(page_size)
                        .ok_or(DriverError::LengthOutOfRange)?,
                )
                .ok_or(DriverError::LengthOutOfRange)?;
            (array, pages)
        } else {
            (0, 0)
        };
        Ok(Self {
            dcbaa,
            erst,
            command_ring,
            event_segment,
            input_ctx,
            output_ctx,
            ep0_ring,
            ctrl_data,
            scratchpad_array,
            scratchpad_pages,
            scratchpad_count,
            page_size,
            ctx_size,
            base: 0,
            total: packer.next,
        })
    }

    /// The same layout moved to the shared chunk granted at `base` (a
    /// [`DmaBank::grow`] base offset): every offset becomes absolute in
    /// the bank's offset space. The scratchpad offsets are moved only when
    /// scratchpad is in use, preserving their "meaningful only when
    /// non-zero-count" contract.
    fn rebased(self, base: usize) -> Self {
        let (scratchpad_array, scratchpad_pages) = if self.scratchpad_count > 0 {
            (self.scratchpad_array + base, self.scratchpad_pages + base)
        } else {
            (0, 0)
        };
        Self {
            dcbaa: self.dcbaa + base,
            erst: self.erst + base,
            command_ring: self.command_ring + base,
            event_segment: self.event_segment + base,
            input_ctx: self.input_ctx + base,
            output_ctx: self.output_ctx + base,
            ep0_ring: self.ep0_ring + base,
            ctrl_data: self.ctrl_data + base,
            scratchpad_array,
            scratchpad_pages,
            base,
            ..self
        }
    }

    /// Offset of context `index` inside the input context (§6.2.5:
    /// index 0 is the input control context, 1 the slot context, and
    /// `1 + dci` the endpoint contexts).
    fn input_ctx_entry(&self, index: usize) -> usize {
        self.input_ctx + index * self.ctx_size
    }
}

/// Default-control-endpoint max packet size *assumed* for a protocol
/// speed ID before the device descriptor reports the real
/// `bMaxPacketSize0` (USB2 §5.5.3, USB3 §9.6.6). Full speed's 64-byte
/// worst case holds only for the one-packet prefix read
/// ([`DEVICE_DESCRIPTOR_PREFIX_LEN`]); a full-speed device may legally
/// use 8/16/32, so any longer transfer must wait for the Evaluate
/// Context fix-up ([`ep0_max_packet_from_descriptor`]).
const fn ep0_max_packet(speed: u8) -> Result<u32, DriverError> {
    match speed {
        SPEED_LOW => Ok(8),
        SPEED_FULL | SPEED_HIGH => Ok(64),
        SPEED_SUPER => Ok(512),
        _ => Err(DriverError::DeviceFault),
    }
}

/// Byte length of the device-descriptor prefix read before the default
/// control endpoint's real max packet size is known: bytes 0..8 of the
/// descriptor, ending at `bMaxPacketSize0` (USB 2.0 §9.6.1). Eight bytes
/// is a single packet at the smallest legal EP0 size, so the read
/// completes identically whatever size the device actually uses.
const DEVICE_DESCRIPTOR_PREFIX_LEN: usize = 8;

/// Validate a device descriptor's `bMaxPacketSize0` against the protocol
/// speed and return the default control endpoint's max packet size in
/// bytes: low speed fixes 8, full speed allows 8/16/32/64, high speed
/// fixes 64 (USB 2.0 §5.5.3), and `SuperSpeed` encodes its fixed 512 as
/// the exponent 9 (USB 3.2 §9.6.1).
///
/// # Errors
///
/// * [`DriverError::BadMagic`] for a value the speed does not permit —
///   a forged or corrupt reply.
/// * [`DriverError::DeviceFault`] for a speed ID this driver does not
///   model.
pub(crate) fn ep0_max_packet_from_descriptor(
    speed: u8,
    b_max_packet0: u8,
) -> Result<u32, DriverError> {
    let valid = match speed {
        SPEED_LOW => b_max_packet0 == 8,
        SPEED_FULL => matches!(b_max_packet0, 8 | 16 | 32 | 64),
        SPEED_HIGH => b_max_packet0 == 64,
        SPEED_SUPER => {
            return if b_max_packet0 == 9 {
                Ok(512)
            } else {
                Err(DriverError::BadMagic)
            }
        }
        _ => return Err(DriverError::DeviceFault),
    };
    if valid {
        Ok(u32::from(b_max_packet0))
    } else {
        Err(DriverError::BadMagic)
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

/// The 8-byte SETUP payload of the HID `SET_PROTOCOL(report)` class request
/// to `interface` (USB HID 1.11 §7.2.6): `wValue` = `1` (report protocol).
///
/// Report protocol is the mode the host uses once it has parsed the device's
/// Report Descriptor. It is the protocol default after reset, but a device
/// left in boot protocol by prior firmware is put back explicitly. Crucially
/// some devices honour `SET_IDLE` — reporting only on change instead of
/// streaming a duplicate every polling interval — only in report protocol, so
/// running report protocol is what lets an idle device fall quiet on the bus.
const fn setup_set_protocol_report(interface: u8) -> [u8; 8] {
    [0x21, 0x0B, 0x01, 0x00, interface, 0x00, 0x00, 0x00]
}

/// The 8-byte SETUP payload of the HID `GET_PROTOCOL` class request to
/// `interface` (USB HID 1.11 §7.2.5): one byte back, `0` = boot protocol,
/// `1` = report protocol.
///
/// `SET_PROTOCOL` is an *optional* request — a device may STALL it, or accept
/// and ignore it — so the mode a device is actually in is a thing to read, not
/// to assume. Boot-decoding an interface that stayed in report protocol reads
/// each report's leading Report ID byte as the boot report's modifier byte,
/// fabricating held modifiers and key usages out of unrelated collections'
/// traffic.
const fn setup_get_protocol(interface: u8) -> [u8; 8] {
    [0xA1, 0x03, 0x00, 0x00, interface, 0x00, 0x01, 0x00]
}

/// `GET_PROTOCOL`'s answer for boot protocol (USB HID 1.11 §7.2.5).
const HID_PROTOCOL_BOOT: u8 = 0;

/// The 8-byte SETUP payload of the standard `GET_DESCRIPTOR(Report)` request
/// to `interface` for `len` bytes (USB HID 1.11 §7.1.1): `bmRequestType`
/// device-to-host/standard/interface (`0x81`), `bRequest` `GET_DESCRIPTOR`
/// (`0x06`), `wValue` = report descriptor type ([`DESC_TYPE_REPORT`]) in the
/// high byte and index `0` in the low byte, `wIndex` the interface.
const fn setup_get_report_descriptor(interface: u8, len: u16) -> [u8; 8] {
    let l = len.to_le_bytes();
    [
        0x81,
        0x06,
        0x00,
        DESC_TYPE_REPORT,
        interface,
        0x00,
        l[0],
        l[1],
    ]
}

/// The 8-byte SETUP payload of the HID `SET_IDLE` class request to
/// `interface` with an *indefinite* idle duration for **all** reports
/// (USB HID 1.11 §7.2.4): `bmRequestType` host-to-device/class/interface
/// (`0x21`), `bRequest` `SET_IDLE` (`0x0A`), `wValue` = duration `0` in the
/// high byte and report id `0` (all reports) in the low byte, `wIndex` the
/// interface, no data stage.
///
/// Duration `0` means the endpoint reports **only when the report data
/// changes** and NAKs otherwise. Without it a boot device is free to stream
/// a fresh report every polling interval whether or not anything changed —
/// so a mouse floods the controller with duplicate reports (and interrupts)
/// forever after the first movement, and a keyboard auto-repeats a held key.
/// Setting the idle duration to indefinite is what makes an idle HID device
/// quiescent on the bus.
const fn setup_set_idle_indefinite(interface: u8) -> [u8; 8] {
    [0x21, 0x0A, 0x00, 0x00, interface, 0x00, 0x00, 0x00]
}

/// The 8-byte SETUP payload of `GET_DESCRIPTOR(configuration, 0)` for
/// `len` bytes (USB 2.0 §9.4.3): descriptor type `0x02` in the high
/// byte of `wValue`, configuration index `0` in the low byte.
const fn setup_get_configuration_descriptor(len: u16) -> [u8; 8] {
    let l = len.to_le_bytes();
    [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, l[0], l[1]]
}

/// The 8-byte SETUP payload of a standard `CLEAR_FEATURE(ENDPOINT_HALT)`
/// targeting endpoint `ep_addr` (USB 2.0 §9.4.1): recipient endpoint,
/// feature selector `ENDPOINT_HALT` (0), resetting the endpoint's
/// device-side halt and data toggle after a STALL.
const fn setup_clear_endpoint_halt(ep_addr: u8) -> [u8; 8] {
    [0x02, 0x01, 0x00, 0x00, ep_addr, 0x00, 0x00, 0x00]
}

/// `bDescriptorType` of a configuration descriptor (USB 2.0 §9.4
/// Table 9-5).
const DESC_TYPE_CONFIGURATION: u8 = 0x02;

/// `bDescriptorType` of an interface descriptor.
const DESC_TYPE_INTERFACE: u8 = 0x04;

/// `bDescriptorType` of a HID class descriptor (USB HID 1.11 §7.1): it
/// follows an interface descriptor and carries the length of the interface's
/// Report Descriptor in its `wDescriptorLength` field.
const DESC_TYPE_HID: u8 = 0x21;

/// Byte offset of `wDescriptorLength` within a HID class descriptor (USB HID
/// 1.11 §6.2.1): after `bLength`, `bDescriptorType`, `bcdHID` (2),
/// `bCountryCode`, `bNumDescriptors`, and the first `bDescriptorType`.
const HID_DESC_REPORT_LEN_OFFSET: usize = 7;

/// `bDescriptorType` of a HID Report descriptor (USB HID 1.11 §7.1.1),
/// requested with the standard `GET_DESCRIPTOR(Report)`.
const DESC_TYPE_REPORT: u8 = 0x22;

/// `bDescriptorType` of an endpoint descriptor (USB 2.0 §9.4 Table 9-5).
const DESC_TYPE_ENDPOINT: u8 = 0x05;

/// Byte length of an endpoint descriptor (USB 2.0 §9.6.6).
const ENDPOINT_DESCRIPTOR_LEN: usize = 7;

/// `bmAttributes` transfer-type mask and the Interrupt and Bulk transfer
/// types (USB 2.0 §9.6.6 Table 9-13).
const ENDPOINT_ATTR_TYPE_MASK: u8 = 0x03;
const ENDPOINT_ATTR_INTERRUPT: u8 = 0x03;
const ENDPOINT_ATTR_BULK: u8 = 0x02;

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
/// would break a following EP0 transfer (`UsbDevice::attach_root_port`).
/// Held as the top byte of the 24-bit class triple ([`InterfaceInfo`]).
const INTERFACE_CLASS_HID: u32 = 0x03;

/// `bInterfaceProtocol` of a HID **keyboard** (USB HID 1.11 §4.2), the low
/// byte of the 24-bit class triple.
const INTERFACE_PROTOCOL_KEYBOARD: u32 = 0x01;

/// `bInterfaceProtocol` of a HID **mouse** (USB HID 1.11 §4.3), the low byte
/// of the 24-bit class triple. A keyboard is `0x01`.
const INTERFACE_PROTOCOL_MOUSE: u32 = 0x02;

/// `bDeviceClass` of a USB hub (USB 2.0 §11.23.1). The Pi 4B's onboard
/// `2109:3431` VIA Labs hub reports this, so the keyboard plugged into a
/// USB-A port is a device *downstream* of the hub, not on a root-hub
/// port — reaching it requires walking the hub (`plans/PI.md`).
const DEVICE_CLASS_HUB: u8 = 0x09;

/// `bDescriptorType` of a USB 2.0 hub class descriptor (USB 2.0
/// §11.23.2.1), requested with a class `GET_DESCRIPTOR`.
const DESC_TYPE_HUB: u8 = 0x29;

/// `bDescriptorType` of the **`SuperSpeed`** hub class descriptor (USB 3.2
/// §10.15.2.1). A `SuperSpeed` hub serves *only* this descriptor — it
/// STALLs a request for the USB 2.0 [`DESC_TYPE_HUB`] one — so the read
/// must select the type by the hub's own protocol speed.
const DESC_TYPE_SS_HUB: u8 = 0x2A;

/// Byte length of the `SuperSpeed` hub descriptor (USB 3.2 §10.15.2.1): a
/// fixed-size descriptor, unlike the USB 2.0 one whose tail varies with
/// the port count.
const SS_HUB_DESC_LEN: usize = 12;

/// Hub class request `SET_HUB_DEPTH` (USB 3.2 §10.16.2.7, Table 10-8):
/// a `SuperSpeed` hub must be told its tier depth (the number of hubs
/// between it and the root port) before it can decode the route string
/// in downstream packet headers; without it, downstream transactions
/// are misrouted.
const HUB_REQUEST_SET_HUB_DEPTH: u8 = 12;

/// Hub class port feature selector `PORT_POWER` (USB 2.0 §11.24.2,
/// Table 11-17): a port-power-controlled hub reports a downstream port
/// disconnected until software sets this.
const PORT_FEATURE_POWER: u8 = 8;

/// Hub class port feature selector `PORT_RESET` (USB 2.0 §11.24.2,
/// Table 11-17): resetting a downstream port enables it and lets the
/// hub establish the device's speed (and, for a full/low-speed device,
/// its transaction translator) before the device is addressed.
const PORT_FEATURE_RESET: u8 = 4;

/// Hub class port feature selectors for the latched port-change bits (USB
/// 2.0 §11.24.2, Table 11-17). A hub keeps its status-change endpoint
/// asserting a report for a port until **every** latched change on it is
/// cleared with a class `CLEAR_FEATURE`; clearing only `C_PORT_CONNECTION`
/// while a `C_PORT_RESET`/`C_PORT_ENABLE` latched by enumeration remains set
/// leaves the port flagged forever, so the watch re-fires endlessly on a
/// stale change. [`UsbDevice::clear_hub_port_changes`] clears each set one.
const PORT_FEATURE_C_CONNECTION: u8 = 16;
const PORT_FEATURE_C_ENABLE: u8 = 17;
const PORT_FEATURE_C_SUSPEND: u8 = 18;
const PORT_FEATURE_C_OVER_CURRENT: u8 = 19;
const PORT_FEATURE_C_RESET: u8 = 20;

/// `wPortStatus` bit: Current Connect Status (USB 2.0 §11.24.2.7.1).
const PORT_STATUS_CONNECT: u16 = 1 << 0;

/// `wPortChange` bits the hub latches and reports in its status-change
/// endpoint bitmap until cleared (USB 2.0 §11.24.2.7.2): Connect Status,
/// Port Enable/Disable, Suspend, Over-Current, and Reset change. Every set
/// bit must be cleared (its [`PORT_FEATURE_C_CONNECTION`]-family selector)
/// or the hub keeps re-asserting the port's status-change report.
const PORT_CHANGE_CONNECT: u16 = 1 << 0;
const PORT_CHANGE_ENABLE: u16 = 1 << 1;
const PORT_CHANGE_SUSPEND: u16 = 1 << 2;
const PORT_CHANGE_OVER_CURRENT: u16 = 1 << 3;
const PORT_CHANGE_RESET: u16 = 1 << 4;

/// Each latched `wPortChange` bit paired with the `CLEAR_FEATURE` selector
/// that clears it, so a port's whole change set is drained in one pass.
const PORT_CHANGE_FEATURES: [(u16, u8); 5] = [
    (PORT_CHANGE_CONNECT, PORT_FEATURE_C_CONNECTION),
    (PORT_CHANGE_ENABLE, PORT_FEATURE_C_ENABLE),
    (PORT_CHANGE_SUSPEND, PORT_FEATURE_C_SUSPEND),
    (PORT_CHANGE_OVER_CURRENT, PORT_FEATURE_C_OVER_CURRENT),
    (PORT_CHANGE_RESET, PORT_FEATURE_C_RESET),
];

/// `SuperSpeed`-hub `wPortChange` bits and `CLEAR_FEATURE` selectors (USB
/// 3.2 §10.16.2.6, Table 10-12): the enable/suspend changes are reserved,
/// and three new latches exist — warm (BH) reset done, a port link-state
/// transition, and a link-configuration error. Every latched bit must be
/// cleared or the hub keeps re-asserting the port's status-change report,
/// exactly as on a USB 2.0 hub.
const PORT_FEATURE_C_LINK_STATE: u8 = 25;
const PORT_FEATURE_C_CONFIG_ERROR: u8 = 26;
const PORT_FEATURE_C_BH_RESET: u8 = 29;
const PORT_CHANGE_BH_RESET: u16 = 1 << 5;
const PORT_CHANGE_LINK_STATE: u16 = 1 << 6;
const PORT_CHANGE_CONFIG_ERROR: u16 = 1 << 7;
const SS_PORT_CHANGE_FEATURES: [(u16, u8); 6] = [
    (PORT_CHANGE_CONNECT, PORT_FEATURE_C_CONNECTION),
    (PORT_CHANGE_OVER_CURRENT, PORT_FEATURE_C_OVER_CURRENT),
    (PORT_CHANGE_RESET, PORT_FEATURE_C_RESET),
    (PORT_CHANGE_BH_RESET, PORT_FEATURE_C_BH_RESET),
    (PORT_CHANGE_LINK_STATE, PORT_FEATURE_C_LINK_STATE),
    (PORT_CHANGE_CONFIG_ERROR, PORT_FEATURE_C_CONFIG_ERROR),
];

/// `wPortStatus` bit: Port Enabled (USB 2.0 §11.24.2.7.1): set by the
/// hub once a port reset completes, the gate the downstream device must
/// pass before it can be addressed.
const PORT_STATUS_ENABLE: u16 = 1 << 1;

/// `wPortStatus` bit: Reset (USB 2.0 §11.24.2.7.1): set while the hub is
/// still driving the downstream port's reset signalling; cleared (with
/// `C_PORT_RESET` latched) once the reset completes.
const PORT_STATUS_RESET: u16 = 1 << 4;

/// `wPortStatus` bit: Low-Speed Device Attached (USB 2.0 §11.24.2.7.1).
const PORT_STATUS_LOW_SPEED: u16 = 1 << 9;

/// `wPortStatus` bit: High-Speed Device Attached (USB 2.0 §11.24.2.7.1).
const PORT_STATUS_HIGH_SPEED: u16 = 1 << 10;

/// xHCI protocol speed ID for a full-speed device (§7.2.1 default speed
/// IDs): the speed of the Pi 4B's keyboard behind the high-speed hub.
const SPEED_FULL: u8 = 1;

/// xHCI protocol speed ID for a low-speed device (§7.2.1).
const SPEED_LOW: u8 = 2;

/// xHCI protocol speed ID for a high-speed device (§7.2.1): the speed of
/// the Pi 4B's onboard hub.
pub(crate) const SPEED_HIGH: u8 = 3;

/// xHCI protocol speed ID for a `SuperSpeed` device (§7.2.1).
const SPEED_SUPER: u8 = 4;

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
/// (USB 2.0 §11.24.2.5 / USB 3.2 §10.16.2.4): `bmRequestType = 0xA0`
/// (device-to-host, class, device), `desc_type` ([`DESC_TYPE_HUB`] or
/// [`DESC_TYPE_SS_HUB`], selected by the hub's own protocol speed) in the
/// high byte of `wValue`, for `len` bytes.
const fn setup_get_hub_descriptor(desc_type: u8, len: u16) -> [u8; 8] {
    let l = len.to_le_bytes();
    [0xA0, 0x06, 0x00, desc_type, 0x00, 0x00, l[0], l[1]]
}

/// The 8-byte SETUP payload of the hub class `SET_HUB_DEPTH(depth)`
/// request (USB 3.2 §10.16.2.7): `bmRequestType = 0x20` (host-to-device,
/// class, device), the hub's tier depth in `wValue`, no data stage.
/// Defined only for `SuperSpeed` hubs.
const fn setup_set_hub_depth(depth: u8) -> [u8; 8] {
    [
        0x20,
        HUB_REQUEST_SET_HUB_DEPTH,
        depth,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
    ]
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

/// The 8-byte SETUP payload of `CLEAR_FEATURE(feature)` on a downstream
/// hub `port` (USB 2.0 §11.24.2.2): `bmRequestType = 0x23` (host-to-device,
/// class, other), `feature` in `wValue`, the 1-based `port` in `wIndex`, no
/// data stage. Used to clear a latched port change (e.g.
/// [`PORT_FEATURE_C_CONNECTION`]) once consumed.
const fn setup_clear_port_feature(feature: u8, port: u8) -> [u8; 8] {
    [0x23, 0x01, feature, 0x00, port, 0x00, 0x00, 0x00]
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

/// Whether a hub port's 16-bit `wPortStatus` reports a reset still in
/// progress (USB 2.0 §11.24.2.7.1) — the hub is still driving the reset
/// signalling, so the enable bit is not yet meaningful.
#[must_use]
pub const fn hub_port_resetting(status: u16) -> bool {
    status & PORT_STATUS_RESET != 0
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
        SPEED_LOW
    } else if status & PORT_STATUS_HIGH_SPEED != 0 {
        SPEED_HIGH
    } else {
        SPEED_FULL
    }
}

/// Extend `parent_route` — the Route String of a hub `parent_depth` tiers
/// below the root port — by one tier: the child on that hub's 1-based
/// downstream `port` (xHCI §8.9.1, four bits per tier, least-significant
/// nibble first).
///
/// Fails closed rather than aliasing topology: the Route String holds
/// exactly [`MAX_HUB_DEPTH`] tiers, and a port above 15 cannot be encoded
/// in a nibble (xHCI §8.9.1 caps routable ports at 15).
pub(crate) fn route_for_child(
    parent_route: u32,
    parent_depth: u8,
    port: u8,
) -> Result<u32, DriverError> {
    if parent_depth >= MAX_HUB_DEPTH || port == 0 || port > 15 {
        return Err(DriverError::OutOfRange);
    }
    Ok(parent_route | (u32::from(port) << (4 * u32::from(parent_depth))))
}

/// What a downstream attach enumerated (`UsbDevice::attach_downstream_device`):
/// a served leaf device at its device-table index, or a further hub tier
/// installed at its hub-table index and descended.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum AttachOutcome {
    /// A leaf device now served at the carried device-table index.
    Device(usize),
    /// A hub now installed, watched, and descended at the carried
    /// hub-table index.
    Hub(usize),
}

/// What a root-hub port currently carries (`UsbDevice::root_attachment_on`):
/// the root-attached hub tier installed there, or the directly-attached
/// leaf device, keyed by the port's recorded topology so the root-port
/// scan detaches exactly what vanished and never double-attaches.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum RootAttachment {
    /// The root-attached hub at the carried hub-table index.
    Hub(usize),
    /// The directly-attached device at the carried device-table index.
    Device(usize),
}

/// One interface's descriptor fields this driver needs (USB 2.0 §9.6.3 /
/// §9.6.5), decoded fail-closed from the `GET_DESCRIPTOR(configuration)`
/// bytes — one per default-alternate interface of the configuration
/// ([`Self::decode_all`]), so a composite device's functions are each
/// represented. The interface class is read from the device, never
/// assumed, so each emitted hardware-tree child node carries the honest
/// class.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct InterfaceInfo {
    /// `bConfigurationValue` to select with `SET_CONFIGURATION`.
    pub configuration_value: u8,
    /// `bInterfaceNumber` of this interface (the target of the HID
    /// `SET_PROTOCOL` class request).
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
    /// `wDescriptorLength` from the interface's HID class descriptor — the
    /// byte length of its Report Descriptor to fetch with
    /// `GET_DESCRIPTOR(Report)`. `0` when the interface declares no HID
    /// descriptor (a non-HID interface, or a HID interface whose descriptor
    /// was malformed), in which case the interface falls back to boot
    /// protocol.
    pub report_descriptor_len: u16,
    /// Device Context Index of the interface's first bulk-IN endpoint
    /// (§4.5.1: `2 * endpoint_number + 1`), read from its endpoint
    /// descriptor. `0` when the interface carries none — a DCI of zero
    /// names no device endpoint, so it doubles as "absent".
    pub bulk_in_dci: u8,
    /// `wMaxPacketSize` (bits 0:10) of the bulk-IN endpoint. `0` when
    /// absent.
    pub bulk_in_max_packet: u16,
    /// Device Context Index of the interface's first bulk-OUT endpoint
    /// (§4.5.1: `2 * endpoint_number`). `0` when absent.
    pub bulk_out_dci: u8,
    /// `wMaxPacketSize` (bits 0:10) of the bulk-OUT endpoint. `0` when
    /// absent.
    pub bulk_out_max_packet: u16,
    /// Device Context Index of the interface's **second** bulk-IN
    /// endpoint — a UAS interface carries two IN pipes (status and
    /// data-in). `0` when the interface declares fewer than two.
    pub bulk_in2_dci: u8,
    /// `wMaxPacketSize` (bits 0:10) of the second bulk-IN endpoint. `0`
    /// when absent.
    pub bulk_in2_max_packet: u16,
    /// Device Context Index of the interface's **second** bulk-OUT
    /// endpoint (a UAS interface's command and data-out pipes). `0` when
    /// absent.
    pub bulk_out2_dci: u8,
    /// `wMaxPacketSize` (bits 0:10) of the second bulk-OUT endpoint. `0`
    /// when absent.
    pub bulk_out2_max_packet: u16,
}

impl InterfaceInfo {
    /// Byte length of a configuration descriptor header (USB 2.0
    /// §9.6.3) and of an interface descriptor (§9.6.5).
    const CONFIG_HEADER_LEN: usize = 9;
    const INTERFACE_LEN: usize = 9;

    /// Decode the `GET_DESCRIPTOR(configuration)` bytes into **every**
    /// default-alternate interface of the configuration (up to
    /// [`MAX_INTERFACES`], filled from index `0`): each interface's number
    /// and class triple, its first interrupt-IN endpoint (DCI, max packet
    /// size, `bInterval`), and its first bulk-IN and bulk-OUT endpoints
    /// (DCI, max packet size — a mass-storage interface's data pipes).
    /// Walks the concatenated descriptors by each `bLength` (every endpoint
    /// is read, never assumed). A composite device — a wireless
    /// keyboard+mouse receiver carrying a boot-keyboard interface *and* a
    /// boot-mouse interface on one device — therefore decodes into one
    /// entry per interface, so each can be served and published separately.
    ///
    /// An interface descriptor with a non-zero `bAlternateSetting` is
    /// skipped along with its endpoints (only the default setting is
    /// selected, USB 2.0 §9.6.5), and a HID interface carrying no
    /// interrupt-IN endpoint — malformed, there is nothing to poll for
    /// reports (USB HID 1.11 §4.4) — is dropped so a well-formed sibling
    /// interface is still served rather than the whole device rejected.
    ///
    /// # Errors
    ///
    /// [`DriverError::BadMagic`] for a non-configuration leading
    /// descriptor, a length running off the buffer or below its minimum,
    /// or no decodable interface at all — a forged or corrupt reply.
    pub fn decode_all(buf: &[u8]) -> Result<[Option<Self>; MAX_INTERFACES], DriverError> {
        if buf.len() < Self::CONFIG_HEADER_LEN
            || usize::from(buf[0]) < Self::CONFIG_HEADER_LEN
            || buf[1] != DESC_TYPE_CONFIGURATION
        {
            return Err(DriverError::BadMagic);
        }
        let configuration_value = buf[5];
        let mut out: [Option<Self>; MAX_INTERFACES] = [None; MAX_INTERFACES];
        let mut count = 0usize;
        let mut offset = usize::from(buf[0]);
        // The default-alternate interface whose endpoints are being
        // collected; `None` before the first interface descriptor and
        // inside a skipped alternate setting.
        let mut interface: Option<(u8, u32)> = None;
        let mut int_endpoint: Option<(u8, u16, u8)> = None;
        // `wDescriptorLength` of the current interface's HID descriptor, if it
        // declares one — the length of the Report Descriptor to fetch.
        let mut hid_report_len: Option<u16> = None;
        let mut bulk_in: [Option<(u8, u16)>; 2] = [None; 2];
        let mut bulk_out: [Option<(u8, u16)>; 2] = [None; 2];
        while offset + 2 <= buf.len() {
            let length = usize::from(buf[offset]);
            let end = offset.checked_add(length).ok_or(DriverError::BadMagic)?;
            if length < 2 || end > buf.len() {
                return Err(DriverError::BadMagic);
            }
            match buf[offset + 1] {
                DESC_TYPE_INTERFACE => {
                    if length < Self::INTERFACE_LEN {
                        return Err(DriverError::BadMagic);
                    }
                    Self::flush_interface(
                        configuration_value,
                        &mut interface,
                        &mut int_endpoint,
                        &mut hid_report_len,
                        &mut bulk_in,
                        &mut bulk_out,
                        &mut out,
                        &mut count,
                    );
                    // Only the default alternate setting is served; an
                    // alternate setting's endpoints must never be mistaken
                    // for the default's (USB 2.0 §9.6.5).
                    if buf[offset + 3] == 0 {
                        interface = Some((
                            buf[offset + 2],
                            (u32::from(buf[offset + 5]) << 16)
                                | (u32::from(buf[offset + 6]) << 8)
                                | u32::from(buf[offset + 7]),
                        ));
                    }
                }
                DESC_TYPE_HID if interface.is_some() && hid_report_len.is_none() => {
                    // The HID descriptor's `wDescriptorLength` (its first
                    // Report Descriptor's length); a descriptor too short to
                    // carry the field is ignored, leaving the interface on the
                    // boot-protocol fallback.
                    if length > HID_DESC_REPORT_LEN_OFFSET + 1 {
                        hid_report_len = Some(u16::from_le_bytes([
                            buf[offset + HID_DESC_REPORT_LEN_OFFSET],
                            buf[offset + HID_DESC_REPORT_LEN_OFFSET + 1],
                        ]));
                    }
                }
                DESC_TYPE_ENDPOINT if interface.is_some() => {
                    if length < ENDPOINT_DESCRIPTOR_LEN {
                        return Err(DriverError::BadMagic);
                    }
                    let address = buf[offset + 2];
                    let attributes = buf[offset + 3];
                    let is_in = address & ENDPOINT_ADDR_DIR_IN != 0;
                    let endpoint_number = address & ENDPOINT_ADDR_NUMBER_MASK;
                    let max_packet = u16::from_le_bytes([buf[offset + 4], buf[offset + 5]])
                        & ENDPOINT_MAX_PACKET_MASK;
                    match attributes & ENDPOINT_ATTR_TYPE_MASK {
                        ENDPOINT_ATTR_INTERRUPT if is_in && int_endpoint.is_none() => {
                            int_endpoint =
                                Some((endpoint_number * 2 + 1, max_packet, buf[offset + 6]));
                        }
                        // The first two bulk endpoints per direction are
                        // captured: one pair serves BOT/CBI, a UAS
                        // interface's four pipes need both.
                        ENDPOINT_ATTR_BULK if is_in => {
                            if let Some(slot) = bulk_in.iter_mut().find(|slot| slot.is_none()) {
                                *slot = Some((endpoint_number * 2 + 1, max_packet));
                            }
                        }
                        ENDPOINT_ATTR_BULK if !is_in => {
                            if let Some(slot) = bulk_out.iter_mut().find(|slot| slot.is_none()) {
                                *slot = Some((endpoint_number * 2, max_packet));
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            offset = end;
        }
        Self::flush_interface(
            configuration_value,
            &mut interface,
            &mut int_endpoint,
            &mut hid_report_len,
            &mut bulk_in,
            &mut bulk_out,
            &mut out,
            &mut count,
        );
        if out[0].is_none() {
            return Err(DriverError::BadMagic);
        }
        Ok(out)
    }

    /// Complete the interface being collected by [`Self::decode_all`] into
    /// the output set, clearing the collection state for the next one. A
    /// HID interface with no interrupt-IN endpoint is dropped (malformed —
    /// nothing to poll for reports), and an interface beyond the
    /// [`MAX_INTERFACES`] bound is ignored rather than trusted.
    #[allow(clippy::similar_names)]
    // The `*2` names are the second pipes'
    // own names beside their primaries — deliberate siblings.
    #[allow(clippy::too_many_arguments)] // One decoder's collection state, threaded by reference.
    fn flush_interface(
        configuration_value: u8,
        interface: &mut Option<(u8, u32)>,
        int_endpoint: &mut Option<(u8, u16, u8)>,
        hid_report_len: &mut Option<u16>,
        bulk_in: &mut [Option<(u8, u16)>; 2],
        bulk_out: &mut [Option<(u8, u16)>; 2],
        out: &mut [Option<Self>; MAX_INTERFACES],
        count: &mut usize,
    ) {
        let int = int_endpoint.take();
        let report_descriptor_len = hid_report_len.take().unwrap_or(0);
        let [b_in, b_in2] = core::mem::take(bulk_in);
        let [b_out, b_out2] = core::mem::take(bulk_out);
        let Some((interface_number, class24)) = interface.take() else {
            return;
        };
        let is_hid = class24 >> 16 == INTERFACE_CLASS_HID;
        let (int_dci, int_max_packet, int_b_interval) = match int {
            Some(endpoint) => endpoint,
            None if is_hid => return,
            None => (DCI_CONTROL, 0, 0),
        };
        if *count >= MAX_INTERFACES {
            return;
        }
        let (bulk_in_dci, bulk_in_max_packet) = b_in.unwrap_or((0, 0));
        let (bulk_out_dci, bulk_out_max_packet) = b_out.unwrap_or((0, 0));
        let (bulk_in2_dci, bulk_in2_max_packet) = b_in2.unwrap_or((0, 0));
        let (bulk_out2_dci, bulk_out2_max_packet) = b_out2.unwrap_or((0, 0));
        out[*count] = Some(Self {
            configuration_value,
            interface_number,
            class24,
            int_dci,
            int_max_packet,
            int_b_interval,
            report_descriptor_len,
            bulk_in_dci,
            bulk_in_max_packet,
            bulk_out_dci,
            bulk_out_max_packet,
            bulk_in2_dci,
            bulk_in2_max_packet,
            bulk_out2_dci,
            bulk_out2_max_packet,
        });
        *count += 1;
    }

    /// Whether this interface carries an endpoint this engine serves: a
    /// HID interface with its interrupt-IN endpoint, or an interface with
    /// the bulk endpoint pair. A served interface gets its own device-table
    /// entry and its own published hardware-tree node.
    #[must_use]
    pub const fn is_servable(&self) -> bool {
        (self.is_hid() && self.int_dci != DCI_CONTROL) || self.has_bulk_pair()
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

    /// Whether this is a HID **mouse** interface (`bInterfaceProtocol` = 2,
    /// USB HID 1.11 §4.3). The pointer report rate is capped for such an
    /// interface (its endpoint-context Interval is clamped to the pointer
    /// minimum) so an aggressive gaming mouse cannot flood the HCD and its
    /// class driver with polls.
    #[must_use]
    pub const fn is_mouse(&self) -> bool {
        self.is_hid() && (self.class24 & 0xFF) == INTERFACE_PROTOCOL_MOUSE
    }

    /// Whether this is a HID **keyboard** interface (`bInterfaceProtocol` = 1,
    /// USB HID 1.11 §4.2). A keyboard is driven in the standardised boot
    /// protocol rather than report protocol: it reports only on a key-state
    /// change (so it never causes the idle interrupt storm a report-streaming
    /// mouse does), and the fixed 8-byte boot report the boot decoder consumes
    /// avoids a report-descriptor parse that a composite keyboard's multiple
    /// Report IDs (or an NKRO layout) can misread and silently drop.
    #[must_use]
    pub const fn is_keyboard(&self) -> bool {
        self.is_hid() && (self.class24 & 0xFF) == INTERFACE_PROTOCOL_KEYBOARD
    }

    /// Whether the matched interface carries the bulk-IN **and** bulk-OUT
    /// endpoint pair a bulk protocol needs (a mass-storage Bulk-Only
    /// Transport interface carries exactly this pair, USB MSC BOT §4).
    /// Only such an interface gets its bulk endpoints configured; a lone
    /// bulk endpoint is left unserved rather than half-configured.
    #[must_use]
    pub const fn has_bulk_pair(&self) -> bool {
        self.bulk_in_dci != 0 && self.bulk_out_dci != 0
    }
}

/// Identity of the enumerated device's served interface (HID or bulk),
/// captured during enumeration ([`UsbDevice::bring_up`]) so the bus can emit it as a
/// discovered hardware-tree child node ([`UsbDevice::describe_device`]).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeviceIdentity {
    pub(crate) vendor_id: u16,
    pub(crate) product_id: u16,
    pub(crate) interface_class: u32,
}

/// Direction of a bulk transfer on the enumerated interface's configured
/// bulk endpoints.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum BulkDirection {
    /// Device → host, on a bulk-IN endpoint.
    In,
    /// Host → device, on a bulk-OUT endpoint.
    Out,
}

/// One of an interface's configured bulk endpoints: its direction and
/// whether it is the second endpoint in that direction (a UAS interface
/// carries two per direction; BOT/CBI use only the primaries).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct BulkPipe {
    /// The pipe's data direction.
    pub direction: BulkDirection,
    /// Whether this is the interface's second pipe in that direction.
    pub secondary: bool,
}

impl BulkPipe {
    /// The interface's primary pipe in `direction`.
    pub(crate) const fn primary(direction: BulkDirection) -> Self {
        Self {
            direction,
            secondary: false,
        }
    }

    /// The interface's second pipe in `direction`.
    pub(crate) const fn secondary(direction: BulkDirection) -> Self {
        Self {
            direction,
            secondary: true,
        }
    }
}

/// One retired bulk TD, as reported by [`UsbDevice::poll_bulk`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct BulkComplete {
    /// Which bulk endpoint the TD ran on.
    pub pipe: BulkPipe,
    /// The transfer-ring data slot the TD occupied (the ticket
    /// [`UsbDevice::queue_bulk_in`] / [`UsbDevice::queue_bulk_out`]
    /// returned), pairing the completion with its submission.
    pub slot: usize,
    /// Bytes actually moved, or the per-transfer failure:
    /// [`DriverError::EndpointStalled`] for a TD the device answered with
    /// STALL (or one
    /// the halt recovery dropped — the endpoint is already recovered when
    /// this is delivered), [`DriverError::DeviceFault`] for a hard
    /// controller/device error on this TD.
    pub result: Result<u32, DriverError>,
}

/// The bulk transfer rings configured for one interface: the primary
/// IN/OUT pair every bulk interface carries, plus the second pair a UAS
/// interface's four pipes need.
#[allow(clippy::struct_field_names)] // Every field *is* a ring — the struct
                                     // is exactly the set of an interface's bulk rings.
struct BulkRings {
    in_ring: ProducerRing,
    out_ring: ProducerRing,
    in2_ring: Option<ProducerRing>,
    out2_ring: Option<ProducerRing>,
}

/// The endpoint rings configured for one planned interface, held until
/// the device-table entries are installed after the EP0 transfers of
/// enumeration complete.
struct ConfiguredRings {
    /// The interrupt-IN transfer ring: a HID interface's report endpoint,
    /// or a bulk interface's CBI completion endpoint.
    int_ring: Option<ProducerRing>,
    /// The bulk transfer rings, for a bulk interface.
    bulk_rings: Option<BulkRings>,
}

/// Parked-completion capacity: every data slot of all four bulk rings
/// could complete while a synchronous EP0 transfer or command is awaiting
/// its own event, so the FIFO holds the worst case. A protocol working
/// set, not a scalable capacity.
const BULK_QUEUE_CAP: usize = 4 * BULK_SLOTS;

/// A fixed-capacity FIFO over a circular buffer — the parked bulk
/// completions and halt-dropped TD records, whose worst case is bounded by
/// the bulk rings' in-flight capacity ([`BULK_QUEUE_CAP`]).
struct Fifo<T: Copy, const N: usize> {
    items: [Option<T>; N],
    head: usize,
    len: usize,
}

impl<T: Copy, const N: usize> Fifo<T, N> {
    const fn new() -> Self {
        Self {
            items: [None; N],
            head: 0,
            len: 0,
        }
    }

    /// Append `item`.
    ///
    /// # Errors
    ///
    /// [`DriverError::Busy`] when full — with capacity sized to the rings'
    /// in-flight bound this means a controller posted more completions than
    /// TDs were queued, surfaced rather than absorbed.
    fn push(&mut self, item: T) -> Result<(), DriverError> {
        if self.len == N {
            return Err(DriverError::Busy);
        }
        self.items[(self.head + self.len) % N] = Some(item);
        self.len += 1;
        Ok(())
    }

    /// Remove and return the oldest item, `None` when empty.
    fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let item = self.items[self.head].take();
        self.head = (self.head + 1) % N;
        self.len -= 1;
        item
    }
}

/// One interrupt-IN report captured off the controller's transfer ring and
/// buffered in the engine until a class-driver URB collects it.
///
/// The report bytes are copied out of the DMA report slot at capture time so
/// the slot can be re-armed immediately: device polling is thereby decoupled
/// from how promptly the class driver (or this host-controller driver) is
/// scheduled to consume it.
#[derive(Copy, Clone)]
struct Report {
    /// The report bytes; only the first [`Self::len`] are valid.
    data: [u8; REPORT_LEN],
    /// Valid byte count in [`Self::data`] (never exceeds [`REPORT_LEN`]).
    len: u8,
}

/// Buffered interrupt-IN reports held per device between the controller
/// capturing them and the class driver collecting them.
///
/// The controller keeps [`INT_ARM_DEPTH`] transfers armed, so one drain can
/// capture that many completions; this buffer additionally covers a class
/// driver that stays several report intervals behind (a full ring's worth of
/// backlog) before the oldest report is dropped. A bounded protocol working
/// set, not a scalable capacity: a genuinely dead consumer cannot make the
/// engine hold unbounded memory, and a live one catching up sees the most
/// recent device state. Sized at least as deep as the armed depth so a single
/// drain never overflows.
pub const REPORT_QUEUE_CAP: usize = 16;

const _: () = assert!(REPORT_QUEUE_CAP >= INT_ARM_DEPTH);

/// Upper bound on transfer completions one [`UsbDevice::drain_events`] pass
/// consumes before yielding.
///
/// The loop exits as soon as the shared event ring is empty; this is only a
/// safety cap against a controller that never stops presenting events. It
/// covers the worst case of every served slot having its full interrupt and
/// bulk depth armed at once, so a legitimate backlog always drains in a single
/// pass.
const EVENT_DRAIN_BOUND: usize = (XHCI_MAX_SLOTS + 1) * (INT_ARM_DEPTH + BULK_QUEUE_CAP);

/// Encode the xHCI endpoint-context Interval (§6.2.3.6, Table 6-12) for
/// an interrupt endpoint reporting `b_interval` at protocol `speed`.
///
/// High-/`SuperSpeed` `bInterval` is already a `2^(n-1)·125µs` exponent, so
/// the context Interval is `bInterval - 1` (clamped 0..=15). Full-/low-speed
/// `bInterval` is in frames (1 ms): converted to 125µs microframes (×8)
/// and reduced to its log2 exponent, clamped to the 3..=10 the periodic
/// scheduler accepts. Derived per-endpoint, not hard-coded.
pub(crate) fn interrupt_interval(speed: u8, b_interval: u8) -> u32 {
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

/// The highest report rate a pointer (mouse) endpoint is polled at.
///
/// A cursor needs no more than ~100 updates/second to feel immediate;
/// polling faster only multiplies controller interrupts and the per-report
/// HCD→class-driver IPC round-trip for no user benefit — a 1000 Hz mouse
/// otherwise woke both processes a thousand times a second while moving. The
/// cap only *lowers* an aggressive endpoint's rate; a mouse that already
/// polls slower keeps its own interval.
const POINTER_MAX_REPORT_HZ: u32 = 100;

/// The xHCI endpoint-context Interval exponent (period = `2^Interval · 125µs`,
/// §6.2.3.6) for the fastest poll that still respects
/// [`POINTER_MAX_REPORT_HZ`]: the smallest exponent whose period is at least
/// `1 / POINTER_MAX_REPORT_HZ`. Derived from the rate, not hard-coded — at
/// 100 Hz this is `7` (128 microframes = 16 ms, ≈62.5 Hz, the nearest
/// power-of-two period at or under the cap). An interrupt endpoint's Interval
/// for a mouse is raised to at least this, never lowered below the device's
/// own.
pub(crate) const fn pointer_min_interval() -> u32 {
    // Longest acceptable period per report, in 125µs microframes.
    let min_microframes = 1_000_000 / POINTER_MAX_REPORT_HZ / 125;
    let mut exponent = 0;
    while (1u32 << exponent) < min_microframes {
        exponent += 1;
    }
    exponent
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
/// A device directly on a root-hub port carries route string `0` and no
/// TT. A full/low-speed device behind a high-speed hub additionally
/// names that hub's slot and downstream port as its TT, so the
/// controller splits its transactions (`speed_needs_tt`).
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
/// Slot context dword 0 **Context Entries** field shift (§6.2.2): the index
/// of the last valid endpoint context in the device context. Raised when an
/// endpoint at a higher DCI (e.g. the hub's status-change endpoint) is added.
const SLOT_CTX_CONTEXT_ENTRIES_SHIFT: u32 = 27;
/// Slot context dword 0 **Context Entries** field mask (five bits).
const SLOT_CTX_CONTEXT_ENTRIES_MASK: u32 = 0x1F << SLOT_CTX_CONTEXT_ENTRIES_SHIFT;
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
fn publish<M: DmaBank>(
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

/// The step the most recent enumeration last entered, a breadcrumb so a
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
    /// `GET_DESCRIPTOR(Report)` control transfer (HID 1.11 §7.1.1), read per
    /// HID interface to run it in report protocol.
    GetReportDescriptor = 8,
    /// HID `SET_PROTOCOL` class request (HID 1.11 §7.2.6).
    SetProtocol = 9,
    /// HID `SET_IDLE(indefinite)` class request (HID 1.11 §7.2.4).
    SetIdle = 10,
    /// HID `GET_PROTOCOL` class request (HID 1.11 §7.2.5), reading back which
    /// protocol the device is actually in rather than assuming the optional
    /// `SET_PROTOCOL` took effect.
    GetProtocol = 12,
    /// Enumeration completed: the device is configured and ready for a class URB.
    Configured = 11,
}

impl EnumStage {
    /// Raw discriminant, for an allocation-free diagnostic log.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Whether this is a step of *establishing the device's control pipe* —
    /// assigning its address and reading its descriptors — as opposed to the
    /// later configuration and HID class requests.
    ///
    /// A transaction fault in this phase is a device disturbed while it is
    /// still being brought up (see [`ENUM_ATTEMPTS`]): it never received the
    /// request, so it stays in Default state and a fresh slot cleanly
    /// re-drives it. A transaction fault *after* this phase — on
    /// `SET_CONFIGURATION`/`CONFIGURE_ENDPOINT` or an (optional) HID class
    /// request — is not re-driven here: the pipe is already up, so it is
    /// surfaced as the genuine fault it is (a STALL on the optional class
    /// requests is separately tolerated where they are issued).
    const fn is_pipe_bringup(self) -> bool {
        matches!(
            self,
            Self::EnableSlot
                | Self::AddressDevice
                | Self::GetDeviceDescriptor
                | Self::GetConfigDescriptor
        )
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

/// The diagnostics of a failed downstream-port attach, snapshotted at the
/// moment the attach failed — **before** the best-effort latch drain and
/// watch re-arm run their own transfers and overwrite the live
/// [`UsbDevice::enum_stage`] / completion / reject state. The first failure
/// of a service is kept (matching the error the per-port fail-soft scan
/// surfaces); [`UsbDevice::next_hub_change`] and the bring-up walks clear it
/// on entry.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AttachFault {
    /// The 1-based downstream hub port whose attach failed.
    pub port: u8,
    /// The surfaced [`DriverError`].
    pub error: DriverError,
    /// The enumeration step the attach failed in ([`EnumStage::PortReset`]
    /// = the port never reported reset-complete + enabled).
    pub stage: EnumStage,
    /// Raw completion code of the last event the failing transfer observed
    /// (`0` = none — a timeout).
    pub completion: u8,
    /// Raw TRB-type of the last event the failing transfer's wait observed
    /// (`0` = none).
    pub event_type: u8,
    /// Why the failing transfer's event wait rejected (the
    /// [`UsbDevice::last_reject_reason`] vocabulary).
    pub reject: u8,
    /// The raw `wPortStatus` the attach's reset-completion wait last
    /// observed (`0` = none read): at a "port never enabled" fault, the
    /// port's final connect/enable/reset/speed state as the hub reported
    /// it.
    pub port_status: u16,
}

/// The outcome of servicing one topology change — a hub status-change
/// report ([`UsbDevice::next_hub_change`]) or a root-port connect/
/// disconnect ([`UsbDevice::next_root_change`]): the engine reads the
/// changed port, and either a fresh device was enumerated, a served
/// device disconnected, or the change required no topology action.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HubEvent {
    /// No actionable change (no completion pending, or a change on a port
    /// carrying no device this engine tracks).
    None,
    /// A device connected on a downstream or root port and was enumerated
    /// as a fresh device at the carried device index; the HCD emits a new
    /// interface node for it. Re-attach is always a brand-new
    /// enumeration — no prior state is reused.
    Attached(usize),
    /// The device at the carried index disconnected; its slot has been
    /// freed. The HCD retracts the interface node it published.
    Detached(usize),
    /// A **hub** connected on a downstream or root port and was installed,
    /// descended, and watched at the carried hub-table index; any devices
    /// found behind it are already served, so the HCD reconciles its
    /// published nodes against the live device table.
    HubAttached(usize),
    /// The hub at the carried hub-table index disconnected; it and every
    /// device and deeper hub tier behind it have been freed. The HCD
    /// reconciles its published nodes against the live device table.
    HubDetached(usize),
}

/// One addressed, watched USB hub: its slot, its place in the hub tree
/// (parent hub and port, route string, depth), the topology fields its
/// downstream devices inherit (speed, TT coordinates), its layout region,
/// and its status-change endpoint state.
///
/// The engine keeps every hub addressed concurrently — the root-attached
/// hub and each hub plugged into a hub — so each tier's status-change
/// endpoint is watched event-driven and its per-port class requests can be
/// issued at any time.
struct HubState {
    /// The hub's xHCI slot (never `0` while the entry is live).
    slot: u8,
    /// Hub-table index of the parent hub, `None` for the root-attached hub.
    parent: Option<usize>,
    /// The parent hub's 1-based downstream port this hub hangs off, `0`
    /// for a root-attached hub.
    parent_port: u8,
    /// The 1-based root-hub port this tier ultimately hangs off: its own
    /// port for a root-attached hub, the parent's inherited value for a
    /// deeper tier. Carried into every downstream slot context (xHCI
    /// §6.2.2) and read by the root-port connect/disconnect scan
    /// ([`UsbDevice::next_root_change`]).
    root_port: u8,
    /// The hub's own Route String (xHCI §8.9.1): `0` for the root-attached
    /// hub; a downstream hub extends its parent's by one nibble.
    route_string: u32,
    /// Hub tiers above this hub (`0` = root-attached), i.e. the number of
    /// route-string nibbles already in use. Bounded by [`MAX_HUB_DEPTH`].
    depth: u8,
    /// The hub's xHCI protocol speed ID, deciding whether a full/low-speed
    /// device below it uses this hub's transaction translator (high-speed
    /// hub) or inherits this hub's own TT coordinates.
    speed: u8,
    /// Downstream port count from the hub class descriptor.
    num_ports: u8,
    /// TT coordinates carried in this hub's own slot context (§6.2.2):
    /// the nearest high-speed ancestor's `(slot, port)` when this hub is
    /// full/low-speed behind one, else `(0, 0)`. A full/low-speed device
    /// below a non-high-speed hub inherits these.
    tt_hub_slot: u8,
    tt_port: u8,
    /// Offset of the hub slot's output device context: the root
    /// [`Layout::output_ctx`] for the root-attached hub, else the claimed
    /// [`DeviceRegion`]'s.
    output_ctx: usize,
    /// Offset of the hub's default-control-endpoint transfer ring, paired
    /// with [`Self::ep0_ring`]. The root [`Layout::ep0_ring`] for the
    /// root-attached hub, else the claimed [`DeviceRegion`]'s.
    ep0_ring_off: usize,
    /// The [`HubRegion`] holding this hub's status-change ring and report
    /// buffer.
    region: HubRegion,
    /// The device-region table index this hub's contexts live on. A
    /// downstream hub is enumerated on a freshly claimed device region
    /// before it is known to be a hub and keeps it for its lifetime (the
    /// entry is excluded from [`UsbDevice::claim_device_entry`]'s reuse
    /// while claimed, and released on detach); `None` for the
    /// root-attached hub, which lives on the shared chunk's root
    /// structures.
    device_region: Option<usize>,
    /// The hub's default-control-endpoint producer ring, **parked** here
    /// while the hub is not the active control context. `None` while
    /// active.
    ep0_ring: Option<ProducerRing>,
    /// The hub's interrupt-IN status-change endpoint as
    /// `(dci, max_packet, interval)`, captured from its configuration
    /// descriptor during enumeration so the watch can be configured once
    /// the slot is marked a hub. `None` when the hub reported none.
    int_endpoint: Option<(u8, u32, u32)>,
    /// Device Context Index of the armed status-change endpoint. Valid
    /// only while [`Self::int_ring`] is live.
    int_dci: u8,
    /// The status-change endpoint's interrupt-IN producer ring (over the
    /// region's `int_ring`). `None` until the watch is configured and
    /// armed.
    int_ring: Option<ProducerRing>,
    /// A status-change completion observed while another transfer was
    /// awaiting its event, parked for the hub watcher to consume. Only one
    /// status-change transfer is ever armed, so a single slot suffices —
    /// unlike a device's report [`Fifo`], which buffers several in-flight
    /// interrupt-IN completions.
    pending: Option<Trb>,
}

/// One concurrently served, enumerated device: its slot, topology, layout
/// region, endpoint state, and the completions parked for it while another
/// transfer owned the shared event ring.
struct DeviceState {
    /// The device's xHCI slot (never `0` while the entry is live).
    slot: u8,
    /// The hub downstream port the device hangs off (1-based), `0` for a
    /// directly-attached root device.
    hub_port: u8,
    /// The 1-based root-hub port the device ultimately hangs off: the port
    /// itself for a directly-attached device, the tier's inherited value
    /// behind a hub. The root-port scan ([`UsbDevice::next_root_change`])
    /// and the disconnect confirmation ([`UsbDevice::detach_if_device_gone`])
    /// key a directly-attached device's fate off it.
    root_port: u8,
    /// Hub-table index of the hub the device hangs off. Meaningful only
    /// while [`Self::hub_port`] is non-zero; a directly-attached root
    /// device has no parent hub.
    parent_hub: usize,
    /// The [`Layout`] region holding this device's endpoint rings and
    /// buffers.
    region: DeviceRegion,
    /// Offset of the device's output device context (the root region's for
    /// a directly-attached device, else its region's).
    output_ctx: usize,
    /// Offset of the device's default-control-endpoint transfer ring,
    /// paired with [`Self::ep0_ring`].
    ep0_ring_off: usize,
    /// The device's default-control-endpoint producer ring, **parked** here
    /// while the device is not the active control context
    /// ([`UsbDevice::activate_device_control`] /
    /// [`UsbDevice::rest_active_context`]). `None` while active.
    ep0_ring: Option<ProducerRing>,
    /// The served interface's identity, for the emitted hardware-tree node.
    identity: DeviceIdentity,
    /// Device Context Index of the device's interrupt-IN endpoint, read
    /// from its endpoint descriptor during enumeration (§4.5.1).
    /// [`DCI_CONTROL`] when the interface carries none (a bulk interface).
    int_dci: u8,
    /// `wMaxPacketSize` (bits 0:10) of the interrupt-IN endpoint, read from
    /// its endpoint descriptor during enumeration. A single service interval
    /// delivers at most this many bytes (the endpoint's Max ESIT payload for
    /// a boot/report HID device), so it is the length each interrupt-IN
    /// transfer is armed to ([`Self::int_capture_len`]). `0` when the
    /// interface carries no interrupt-IN endpoint.
    ///
    /// Arming a transfer longer than this makes a full/low-speed endpoint's
    /// periodic split (through a high-speed hub's transaction translator)
    /// exceed the per-interval budget the TT scheduled, which the controller
    /// reports as a Split Transaction Error — the metal "keyboard registers
    /// no keypresses" defect for an 8-byte-max-packet keyboard behind the
    /// Pi 4 hub. A directly-attached high-speed device tolerated the
    /// over-length transfer (it short-packets with no split), which is why
    /// only the hub-attached keyboard failed.
    int_max_packet: u16,
    /// Interrupt-IN transfer ring over the region's `int_ring`, live only
    /// for a HID interface.
    int_ring: Option<ProducerRing>,
    /// The parsed HID Report Descriptor for this interface, present when the
    /// device runs in **report protocol** (`SET_IDLE` then quiesces an idle
    /// device instead of it streaming a duplicate report every interval). Each
    /// captured report-protocol report is rewritten into the fixed boot layout
    /// through this map before it is buffered ([`UsbDevice::capture_report_event`]),
    /// so the class drivers keep seeing boot-format reports. `None` when the
    /// interface fell back to boot protocol (the descriptor was unreadable or
    /// unparsable), in which case reports are buffered as delivered.
    report_decode: ReportDecode,
    /// Interrupt-IN reports captured off the controller's ring, buffered
    /// until a class-driver URB collects them. Filled by
    /// [`UsbDevice::capture_report_event`] on every controller interrupt
    /// (independent of whether a URB is outstanding), drained by
    /// [`UsbDevice::next_report`]. Several transfers are armed at once
    /// ([`INT_ARM_DEPTH`]), so more than one completion can be pending —
    /// a queue, not a single slot.
    reports: Fifo<Report, REPORT_QUEUE_CAP>,
    /// A fatal report completion (a device-gone or fail-closed code) recorded
    /// for the next [`UsbDevice::next_report`] to surface, so the HCD confirms
    /// the port and detaches. Set once; taken when surfaced. A capture never
    /// propagates such a fault synchronously, so it can neither fault an
    /// unrelated EP0/command wait that happened to observe the report nor the
    /// shared drain.
    report_fault: Option<DriverError>,
    /// Count of buffered reports dropped because the class driver fell more
    /// than [`REPORT_QUEUE_CAP`] reports behind (a genuinely stalled
    /// consumer). Surfaced for diagnostics; never silently ignored.
    dropped_reports: u64,
    /// The interrupt-IN endpoint completed a transfer with a halting error
    /// code and is stopped by the controller until it is reset. Set by
    /// [`UsbDevice::capture_report_event`] the moment the halt is observed
    /// (which runs re-entrantly from inside a synchronous EP0/command wait,
    /// so it MUST NOT recover the endpoint there); the actual recovery
    /// ([`UsbDevice::recover_interrupt_endpoint`]) is performed later at a
    /// top-level, non-re-entrant point ([`UsbDevice::recover_report_endpoint_if_pending`],
    /// called from [`UsbDevice::next_report`] and [`UsbDevice::pump_reports`]).
    /// Cleared once the endpoint is recovered.
    int_recovery_pending: bool,
    /// A [`UsbDevice::recover_interrupt_endpoint`] for this device's
    /// interrupt-IN endpoint is in progress. Recovery issues Reset Endpoint /
    /// Set TR Dequeue commands and a device-side `CLEAR_FEATURE`, each of
    /// which waits on the shared event ring — during which a fresh interrupt
    /// completion for the *same* endpoint can arrive and reach
    /// [`UsbDevice::capture_report_event`]. This guard makes that re-entrant
    /// capture leave the ring (which recovery is rebuilding) untouched and
    /// merely re-flag [`Self::int_recovery_pending`], so recovery can never
    /// recurse into itself and scramble the ring — the on-metal defect where
    /// hammering a device during USB bring-up killed its class driver.
    int_recovering: bool,
    /// Bulk-IN transfer ring over the region's `bulk_in_ring`, built when
    /// the interface's bulk endpoint pair is configured. `None` otherwise.
    bulk_in_ring: Option<ProducerRing>,
    /// Bulk-OUT transfer ring, as [`Self::bulk_in_ring`].
    bulk_out_ring: Option<ProducerRing>,
    /// The second bulk-IN ring (a UAS interface's second IN pipe), `None`
    /// when the interface declares fewer than two.
    bulk_in2_ring: Option<ProducerRing>,
    /// The second bulk-OUT ring, as [`Self::bulk_in2_ring`].
    bulk_out2_ring: Option<ProducerRing>,
    /// Device Context Index of the configured bulk-IN endpoint (`0` = none).
    bulk_in_dci: u8,
    /// Device Context Index of the configured bulk-OUT endpoint (`0` = none).
    bulk_out_dci: u8,
    /// DCI of the second configured bulk-IN endpoint (`0` = none).
    bulk_in2_dci: u8,
    /// DCI of the second configured bulk-OUT endpoint (`0` = none).
    bulk_out2_dci: u8,
    /// Requested byte length of each in-flight bulk-IN TD, by ring data
    /// slot, so a completion's residual decodes into bytes transferred.
    bulk_in_len: [u32; BULK_SLOTS],
    /// As [`Self::bulk_in_len`], for the bulk-OUT ring.
    bulk_out_len: [u32; BULK_SLOTS],
    /// As [`Self::bulk_in_len`], for the second bulk-IN ring.
    bulk_in2_len: [u32; BULK_SLOTS],
    /// As [`Self::bulk_in_len`], for the second bulk-OUT ring.
    bulk_out2_len: [u32; BULK_SLOTS],
    /// Bulk completions parked while a synchronous EP0 transfer or command
    /// was awaiting its own event. Several bulk TDs can be outstanding at
    /// once, so this is a FIFO — as is the interrupt-IN report buffer
    /// ([`Self::reports`]).
    pending_bulk: Fifo<Trb, BULK_QUEUE_CAP>,
    /// TDs a bulk halt recovery dropped, reported as stalled completions so
    /// every queued transfer is answered, never silently lost.
    aborted_bulk: Fifo<(BulkPipe, usize), BULK_QUEUE_CAP>,
    /// Raw completion code of the most recent interrupt-IN transfer event
    /// this device's report path *rejected* (a non-`Success`/`ShortPacket`
    /// code). Unlike the engine-wide diagnostics this is not reset by a
    /// later control transfer, so it survives the hub
    /// disconnect-confirmation the HCD issues after a report fault — the
    /// only place the controller's verdict on the device's own endpoint can
    /// still be read. `0` until a report has been rejected.
    last_report_fault_code: u8,
    /// Bytes of HID Report Descriptor this interface declared, captured at
    /// enumeration for the metal report diagnostic ([`UsbDevice::hid_enum_diag`]).
    /// `0` when the interface declared none (a bulk interface, or a HID device
    /// with no report descriptor → boot-protocol fallback).
    report_descriptor_len: u16,
    /// The Report Descriptor bytes the interface actually delivered, for the
    /// metal report diagnostic ([`UsbDevice::hid_report_descriptor`]). Shorter
    /// than [`Self::report_descriptor_len`] when the device delivered less
    /// than it declared, empty when it declared none or refused the read.
    report_descriptor: ReportDescriptor,
}

/// A HID interface's Report Descriptor as the device delivered it, bounded by
/// [`REPORT_DESCRIPTOR_LEN`].
///
/// Retained because it is the sole input the report map is derived from: on
/// hardware QEMU cannot model, a map that reads a device's reports at the
/// wrong offsets — or accepts a sibling collection's report as its own — is
/// otherwise undiagnosable from the map alone. It is a device *capability*
/// blob, so unlike a report it carries no keystroke or pointer movement and
/// may be logged.
struct ReportDescriptor {
    bytes: [u8; REPORT_DESCRIPTOR_LEN],
    len: u16,
}

impl ReportDescriptor {
    /// The descriptor of an interface that declared none, or refused the read.
    const fn none() -> Self {
        Self {
            bytes: [0; REPORT_DESCRIPTOR_LEN],
            len: 0,
        }
    }

    /// Retain `bytes`, truncated to the bound.
    fn new(bytes: &[u8]) -> Self {
        let kept = bytes.len().min(REPORT_DESCRIPTOR_LEN);
        let mut descriptor = Self::none();
        descriptor.bytes[..kept].copy_from_slice(&bytes[..kept]);
        descriptor.len = u16::try_from(kept).unwrap_or(0);
        descriptor
    }

    /// The delivered bytes, empty when the interface declared no descriptor.
    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len).min(REPORT_DESCRIPTOR_LEN)]
    }
}

/// How a served HID interface's captured reports are read.
///
/// Three states, not two, because "the device is in boot protocol" and "the
/// device is in report protocol and we have no map for it" are different
/// facts with different safe answers. Collapsing the second into the first is
/// what boot-decodes an ID-prefixed report — reading its leading Report ID
/// byte as the boot modifier byte — and invents held modifiers and key usages
/// out of a sibling collection's traffic.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ReportDecode {
    /// Boot protocol: the reports already carry the fixed boot layout, so they
    /// are buffered as delivered.
    Boot,
    /// Report protocol: each report is rewritten through this map, which also
    /// demuxes the interface's own Report ID from its siblings'.
    Mapped(tairix_hid::HidReportMap),
    /// Neither is safe — the device answered that it is in report protocol and
    /// its descriptor yields no map — so nothing is delivered. A silent
    /// interface is the honest outcome; fabricated input is not.
    Refused,
}

impl ReportDecode {
    /// The map reports are rewritten through, `None` unless mapped.
    const fn map(self) -> Option<tairix_hid::HidReportMap> {
        match self {
            Self::Mapped(map) => Some(map),
            Self::Boot | Self::Refused => None,
        }
    }
}

/// What bringing one HID interface up established: how its reports are read,
/// and the descriptor that decision was made from.
///
/// Grouped because both come from the one `configure_hid_protocol` step and
/// both are recorded on the installed device.
struct HidSetup {
    decode: ReportDecode,
    descriptor: ReportDescriptor,
}

impl HidSetup {
    /// The setup of a non-HID interface: no descriptor, boot layout.
    const fn none() -> Self {
        Self {
            decode: ReportDecode::Boot,
            descriptor: ReportDescriptor::none(),
        }
    }
}

/// One-shot view of an interface's HID enumeration decision, for the
/// host-controller driver to log on metal (`plans/USB.md`; QEMU models no Pi
/// USB, so this is the only window on how a device's reports are read). All
/// fields are derived from what enumeration recorded — never assumed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct HidEnumDiag {
    /// Bytes of HID Report Descriptor the interface declared (`0` = none).
    pub report_descriptor_len: u16,
    /// `true` when the interface runs in **report protocol** (a report map was
    /// parsed); `false` on the boot-protocol fallback.
    pub report_protocol: bool,
    /// `true` when the device answered that it is in report protocol but its
    /// descriptor yields no map, so no report is delivered at all: neither
    /// reading is safe and a silent interface beats fabricated input.
    pub reports_refused: bool,
    /// The parsed report-map summary, `None` on the boot fallback.
    pub map: Option<tairix_hid::ReportMapSummary>,
    /// The interrupt-IN endpoint's `wMaxPacketSize`.
    pub int_max_packet: u16,
    /// The byte length each interrupt-IN transfer is armed to (derived from
    /// `int_max_packet`), so a metal capture shows the on-wire transfer size.
    pub capture_len: u16,
}

impl DeviceState {
    /// Byte length one interrupt-IN transfer is armed to for this device: the
    /// endpoint's `wMaxPacketSize` ([`Self::int_max_packet`]), never more than
    /// the [`CAPTURE_LEN`] capture buffer. A single service interval delivers
    /// at most one packet for a boot/report HID endpoint, so this captures a
    /// whole report while never asking a full/low-speed endpoint's transaction
    /// translator for more than the per-interval budget it scheduled (which
    /// would fault as a Split Transaction Error). Falls back to [`CAPTURE_LEN`]
    /// only if the packet size is unknown, which never happens for a live
    /// interrupt endpoint.
    fn int_capture_len(&self) -> usize {
        let packet = usize::from(self.int_max_packet).min(CAPTURE_LEN);
        if packet == 0 {
            CAPTURE_LEN
        } else {
            packet
        }
    }

    /// The configured DCI of `pipe` (`0` = the pipe does not exist).
    fn bulk_dci(&self, pipe: BulkPipe) -> u8 {
        match (pipe.direction, pipe.secondary) {
            (BulkDirection::In, false) => self.bulk_in_dci,
            (BulkDirection::In, true) => self.bulk_in2_dci,
            (BulkDirection::Out, false) => self.bulk_out_dci,
            (BulkDirection::Out, true) => self.bulk_out2_dci,
        }
    }

    /// The configured pipe whose endpoint is `dci`, `None` when no bulk
    /// pipe of this device uses it.
    fn bulk_pipe_of_dci(&self, dci: u8) -> Option<BulkPipe> {
        if dci == 0 {
            return None;
        }
        [
            BulkPipe::primary(BulkDirection::In),
            BulkPipe::primary(BulkDirection::Out),
            BulkPipe::secondary(BulkDirection::In),
            BulkPipe::secondary(BulkDirection::Out),
        ]
        .into_iter()
        .find(|&pipe| self.bulk_dci(pipe) == dci)
    }

    /// Borrow `pipe`'s transfer ring, `None` when unconfigured.
    fn bulk_ring(&self, pipe: BulkPipe) -> Option<&ProducerRing> {
        match (pipe.direction, pipe.secondary) {
            (BulkDirection::In, false) => self.bulk_in_ring.as_ref(),
            (BulkDirection::In, true) => self.bulk_in2_ring.as_ref(),
            (BulkDirection::Out, false) => self.bulk_out_ring.as_ref(),
            (BulkDirection::Out, true) => self.bulk_out2_ring.as_ref(),
        }
    }

    /// Mutably borrow `pipe`'s transfer ring.
    fn bulk_ring_mut(&mut self, pipe: BulkPipe) -> Option<&mut ProducerRing> {
        match (pipe.direction, pipe.secondary) {
            (BulkDirection::In, false) => self.bulk_in_ring.as_mut(),
            (BulkDirection::In, true) => self.bulk_in2_ring.as_mut(),
            (BulkDirection::Out, false) => self.bulk_out_ring.as_mut(),
            (BulkDirection::Out, true) => self.bulk_out2_ring.as_mut(),
        }
    }

    /// Replace `pipe`'s transfer ring (the halt recovery's rebuild).
    fn set_bulk_ring(&mut self, pipe: BulkPipe, ring: ProducerRing) {
        match (pipe.direction, pipe.secondary) {
            (BulkDirection::In, false) => self.bulk_in_ring = Some(ring),
            (BulkDirection::In, true) => self.bulk_in2_ring = Some(ring),
            (BulkDirection::Out, false) => self.bulk_out_ring = Some(ring),
            (BulkDirection::Out, true) => self.bulk_out2_ring = Some(ring),
        }
    }

    /// The requested length recorded for `pipe`'s ring data `slot`.
    fn bulk_len(&self, pipe: BulkPipe, slot: usize) -> u32 {
        match (pipe.direction, pipe.secondary) {
            (BulkDirection::In, false) => self.bulk_in_len[slot],
            (BulkDirection::In, true) => self.bulk_in2_len[slot],
            (BulkDirection::Out, false) => self.bulk_out_len[slot],
            (BulkDirection::Out, true) => self.bulk_out2_len[slot],
        }
    }

    /// Record the requested length of the TD in `pipe`'s ring data `slot`.
    fn set_bulk_len(&mut self, pipe: BulkPipe, slot: usize, len: u32) {
        match (pipe.direction, pipe.secondary) {
            (BulkDirection::In, false) => self.bulk_in_len[slot] = len,
            (BulkDirection::In, true) => self.bulk_in2_len[slot] = len,
            (BulkDirection::Out, false) => self.bulk_out_len[slot] = len,
            (BulkDirection::Out, true) => self.bulk_out2_len[slot] = len,
        }
    }
}

/// Push one `None` entry onto a growable engine table, fallibly:
/// exhaustion of the bookkeeping heap surfaces as a typed error, never a
/// panic (deterministic OOM).
fn push_free_entry<T>(table: &mut Vec<Option<T>>) -> Result<usize, DriverError> {
    table
        .try_reserve(1)
        .map_err(|_| DriverError::LengthOutOfRange)?;
    table.push(None);
    Ok(table.len() - 1)
}

/// The controller engine serving every enumerated device on one started
/// xHCI controller.
///
/// [`UsbDevice::start`] lays the DMA structures out, programs them
/// through [`Xhci::start`], and leaves the controller running.
/// [`UsbDevice::bring_up`] then enumerates every reachable device into the
/// device table. [`UsbDevice::next_report`] arms one interrupt-IN transfer
/// for the class-driver URB a device is currently serving, and the
/// host-controller driver completes that URB from the controller event.
pub struct UsbDevice<'w, H: XhciHost, M: DmaBank> {
    xhci: Xhci<H>,
    dma: M,
    layout: Layout,
    command_ring: ProducerRing,
    /// The default-control-endpoint transfer ring of the **currently
    /// active** control-context slot ([`Self::slot`]). Initially the root
    /// device's ring; rebound to a device region by
    /// [`Self::rebind_to_device_region`] while a device downstream of the
    /// hub is enumerated or activated, so the hub's ring stays intact in
    /// the DCBAA while EP0 transfers target that device.
    ep0_ring: ProducerRing,
    /// Region offset of [`Self::ep0_ring`] (the active slot's EP0 ring),
    /// for publishing TRBs into it. Either [`Layout::ep0_ring`] (the root
    /// device) or a device region's `ep0_ring` (a downstream device).
    ep0_ring_off: usize,
    /// Region offset of the active slot's output device context, written
    /// into its DCBAA entry by [`Self::address_device`]. Either
    /// [`Layout::output_ctx`] or a device region's `output_ctx`.
    output_ctx_off: usize,
    event_cursor: EventRingCursor,
    /// Whether some root port's change bits may be latched, so
    /// [`Self::next_root_change`] must scan `PORTSC`.
    ///
    /// Armed by the two independent latched sources the controller offers — a
    /// drained Port Status Change Event ([`Self::poll_event`]) and the
    /// `USBSTS.PCD` summary ([`Self::acknowledge_interrupt`]) — and starts set
    /// so bring-up, and a re-enumeration after a controller reset, scan the
    /// ports the firmware may already have left connected. Without it the scan
    /// costs one `PORTSC` read per port on *every* interrupt, which on a PCIe
    /// controller is the most expensive thing on the report path.
    root_change_pending: bool,
    /// Bound on the *register-handshake* polls (`Xhci` open/start/reset
    /// readiness waits) only — the brief, bounded MMIO waits the silicon
    /// dictates. Event waits are parked on [`Self::wait`] and bounded by
    /// wall-clock time, never by this count.
    budget: u32,
    /// The parked event-wait seam every synchronous completion wait and
    /// the connect debounce block through (see [`EventWait`]).
    wait: &'w dyn EventWait,
    /// The **active control context** slot ([`Self::control`] / [`Self::
    /// command`] target). When hubs are addressed this rests on the
    /// root-attached hub's slot; it is another hub's or a served device's
    /// slot only while that hub or device is being enumerated or activated
    /// ([`Self::activate_hub_control`] /
    /// [`Self::activate_device_control`]), then restored to the root hub
    /// by [`Self::rest_active_context`]. With no hub it is simply the root
    /// device's slot.
    slot: u8,
    /// The concurrently served devices, indexed by the device index the
    /// [`HubEvent`]s carry and the per-device transfer paths take. `None`
    /// entries are free. Grows as devices attach ([`Self::claim_device_entry`])
    /// and is bounded only by the controller's reported slot count times the
    /// servable interfaces per slot — a silicon-derived ceiling, never a
    /// hand-picked constant.
    devices: Vec<Option<DeviceState>>,
    /// Each table entry's demand-allocated DMA chunk, index-aligned with
    /// [`Self::devices`]. `Some` from the entry's claim
    /// ([`Self::claim_device_entry`]) until its release — a downstream
    /// hub's contexts keep their entry's region claimed for the hub's
    /// lifetime ([`HubState::device_region`]) even though no served device
    /// occupies the entry.
    regions: Vec<Option<DeviceRegion>>,
    /// Index into [`Self::devices`] of the device that is the active
    /// control context, `None` while a hub (or the root device) is active.
    active_device: Option<usize>,
    /// The addressed hubs, indexed by the hub-table index [`HubState::
    /// parent`] and [`DeviceState::parent_hub`] refer to. Entry `0` is the
    /// root-attached hub; every hub is kept addressed concurrently with
    /// the served devices so each tier's status-change endpoint is watched
    /// and its per-port class requests can be issued. All `None` when the
    /// root device is not a hub (no hub tier). Grows as hub tiers install
    /// ([`Self::claim_hub_entry`]); each entry's status-change watch lives
    /// in its own demand-allocated chunk, released on detach.
    hubs: Vec<Option<HubState>>,
    /// Index into [`Self::hubs`] of the hub that is the active control
    /// context, `None` while a device (or the pre-install enumeration
    /// cursor) is active. At rest this is `Some(0)` (the root hub) when a
    /// hub topology exists.
    active_hub: Option<usize>,
    /// A just-enumerated hub's interrupt-IN status-change endpoint as
    /// `(dci, max_packet, interval)`, captured by
    /// [`Self::finish_enumeration`] (which recognises the hub before any
    /// [`HubState`] exists for it) and consumed by the hub-install path.
    /// `None` between enumerations.
    pending_hub_endpoint: Option<(u8, u32, u32)>,
    /// Slots of devices just freed by hot-removals
    /// ([`Self::detach_device`]), retained so a *trailing* transfer event
    /// the controller still posts for a vanished slot (an in-flight
    /// transfer dropped by the unplug, or a Disable Slot side-effect) is
    /// recognised as stale and drained, never mistaken for a controller
    /// protocol violation. `0` entries are free; the whole set is cleared
    /// once a fresh device enumerates (any trailing completion has long
    /// since arrived by then). Without this, such a stale event matched
    /// neither a device endpoint nor the hub endpoint and faulted the
    /// event-ring consumers, wedging the hub status-change watch so a later
    /// re-plug went unseen. Deduplicated, so it never holds more than the
    /// protocol's 255 slots ([`XHCI_MAX_SLOTS`]).
    freed_slots: Vec<u8>,
    /// The last enumeration step entered, for a
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
    /// event-wait budget elapsed with no event (a genuine timeout).
    last_reject: u8,
    /// Downstream hub ports whose connected device failed enumeration and
    /// was skipped fail-soft by the bring-up walk ([`Self::descend_hub`]),
    /// so the driver above can surface "a device was present but never
    /// served" instead of it looking like an empty port. Reset on each
    /// fresh bring-up walk.
    skipped_ports: u32,
    /// The raw `wPortStatus` the in-progress attach's reset-completion
    /// wait last observed (`0` = none read), feeding
    /// [`AttachFault::port_status`] when the attach fails.
    last_attach_status: u16,
    /// The first failed downstream-port attach of the current service
    /// ([`Self::last_attach_fault`]), snapshotted before the failure
    /// path's own cleanup transfers overwrite the live diagnostics.
    attach_fault: Option<AttachFault>,
}

impl<'w, H: XhciHost, M: DmaBank> UsbDevice<'w, H, M> {
    /// Grow the controller's shared chunk out of `dma`, lay the shared
    /// structures out inside it, program them, and start the controller.
    ///
    /// The chunk is sized **exactly** to the geometry the silicon reports
    /// (`MaxSlots`, context size, scratchpad count and page size); no
    /// per-device memory is reserved here — each device's region is grown
    /// on attach and released on detach, so the served-device count is
    /// bounded by the controller's slots and genuine memory exhaustion,
    /// never a compile-time budget.
    ///
    /// `budget` bounds the register-handshake polls (the brief MMIO
    /// readiness waits the silicon dictates), failing closed on a stuck
    /// controller. Every *event* wait instead parks on `wait` and is
    /// bounded by wall-clock time (`AWAIT_EVENT_BUDGET_US`), so the
    /// engine never spins a core while the controller works.
    ///
    /// The controller's completion interrupter is enabled here (and on
    /// every re-program after a reset), because the engine's own waits
    /// park on that interrupt: the caller must have routed and bound the
    /// controller's interrupt line **before** calling this.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if the granted chunk's device-visible
    ///   base is zero, not 64-byte aligned, or (when the controller needs
    ///   scratchpad) leaves the scratchpad pages off a controller-page
    ///   boundary.
    /// * [`DriverError::LengthOutOfRange`] if the bank cannot supply the
    ///   shared chunk (deterministic OOM — the [`DmaHost`] exhaustion
    ///   convention).
    /// * [`DriverError::DeviceFault`] if the controller does not
    ///   start within `budget` polls.
    ///
    /// [`DmaHost`]: tairix_abi::driver::dma::DmaHost
    pub fn start(
        xhci: Xhci<H>,
        dma: M,
        wait: &'w dyn EventWait,
        budget: u32,
    ) -> Result<Self, DriverError> {
        let mut xhci = xhci;
        let mut dma = dma;
        let layout = Layout::new(
            xhci.max_slots(),
            xhci.csz(),
            xhci.max_scratchpad_buffers(),
            xhci.page_size(),
        )?;
        let base = dma.grow(layout.total)?;
        let layout = layout.rebased(base);
        let phys = dma.phys_of(base)?;
        if phys == 0 || phys % 64 != 0 {
            return Err(DriverError::OutOfRange);
        }
        // Each scratchpad buffer must land on a controller-page boundary
        // in the device address space (xHCI §4.20 / §6.6); fail closed on
        // a chunk the bank could not place page-aligned.
        if layout.scratchpad_count > 0
            && dma.phys_of(layout.scratchpad_pages)? % layout.page_size as u64 != 0
        {
            return Err(DriverError::OutOfRange);
        }

        let (command_ring, ep0_ring, event_cursor) =
            Self::program_and_start(&mut xhci, &mut dma, &layout, budget)?;

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
            event_cursor,
            root_change_pending: true,
            budget,
            wait,
            slot: 0,
            devices: Vec::new(),
            regions: Vec::new(),
            active_device: None,
            hubs: Vec::new(),
            active_hub: None,
            pending_hub_endpoint: None,
            freed_slots: Vec::new(),
            stage: EnumStage::Scan,
            last_completion: 0,
            last_event_type: 0,
            last_reject: 0,
            skipped_ports: 0,
            last_attach_status: 0,
            attach_fault: None,
        })
    }

    /// Zero the DMA region, build the command and root EP0 producer rings
    /// and the event-ring cursor, reserve the controller's scratchpad
    /// buffers, and start the controller.
    ///
    /// Factored out of [`Self::start`] so the controller re-bring-up after a
    /// device hot-removal ([`Self::reset_and_reenumerate`]) re-programs the
    /// *same* held DMA region and register window identically, rather than
    /// duplicating the sequence. The hub status-change interrupt ring and
    /// every per-device endpoint ring are built lazily when their device is
    /// configured, not here.
    ///
    /// # Errors
    ///
    /// As [`Self::start`].
    fn program_and_start(
        xhci: &mut Xhci<H>,
        dma: &mut M,
        layout: &Layout,
        budget: u32,
    ) -> Result<(ProducerRing, ProducerRing, EventRingCursor), DriverError> {
        let zeros = [0u8; 64];
        let mut offset = 0;
        while offset < layout.total {
            let chunk = (layout.total - offset).min(zeros.len());
            dma.write(layout.base + offset, &zeros[..chunk])?;
            offset += chunk;
        }

        // The single event ring segment table entry: segment base and
        // size in TRBs.
        let event_phys = dma.phys_of(layout.event_segment)?;
        let segment_trbs = u32::try_from(RING_TRBS).map_err(|_| DriverError::LengthOutOfRange)?;
        let mut erst = [0u8; 16];
        erst[..8].copy_from_slice(&event_phys.to_le_bytes());
        erst[8..12].copy_from_slice(&segment_trbs.to_le_bytes());
        dma.write(layout.erst, &erst)?;

        let mut make_ring = |offset: usize| -> Result<ProducerRing, DriverError> {
            let (ring, link) = ProducerRing::new(RING_TRBS, dma.phys_of(offset)?)?;
            dma.write(offset + ring.link_slot() * trb::TRB_LEN, &link.to_bytes())?;
            Ok(ring)
        };
        let command_ring = make_ring(layout.command_ring)?;
        let ep0_ring = make_ring(layout.ep0_ring)?;
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
                let page = dma.phys_of(layout.scratchpad_pages + index * layout.page_size)?;
                dma.write(layout.scratchpad_array + index * 8, &page.to_le_bytes())?;
            }
            let array = dma.phys_of(layout.scratchpad_array)?;
            dma.write(layout.dcbaa, &array.to_le_bytes())?;
        }

        xhci.start(
            &DmaProgram {
                dcbaap: dma.phys_of(layout.dcbaa)?,
                command_ring: dma.phys_of(layout.command_ring)?,
                erst: dma.phys_of(layout.erst)?,
                event_segment: event_phys,
            },
            budget,
        )?;

        // The engine's synchronous waits park on the controller's completion
        // interrupt, so interrupt generation is part of starting the
        // controller — enabled here so a cold boot and a post-reset
        // re-program share the one definition. The caller has already bound
        // the interrupt line, so the first completion has a kernel-owned
        // line to latch onto.
        xhci.enable_interrupter()?;

        Ok((command_ring, ep0_ring, event_cursor))
    }

    /// The device table entry at `index`, when live.
    fn device(&self, index: usize) -> Option<&DeviceState> {
        self.devices.get(index).and_then(Option::as_ref)
    }

    /// The mutable device table entry at `index`, when live.
    fn device_mut(&mut self, index: usize) -> Option<&mut DeviceState> {
        self.devices.get_mut(index).and_then(Option::as_mut)
    }

    /// The hub table entry at `hub_index`, when live.
    fn hub(&self, hub_index: usize) -> Option<&HubState> {
        self.hubs.get(hub_index).and_then(Option::as_ref)
    }

    /// The mutable hub table entry at `hub_index`, when live.
    fn hub_mut(&mut self, hub_index: usize) -> Option<&mut HubState> {
        self.hubs.get_mut(hub_index).and_then(Option::as_mut)
    }

    /// Claim a hub-table entry: reuse a free one or grow the table. The
    /// table is bounded by the controller's own slot count — every tracked
    /// hub holds an xHCI slot — so growth is silicon-derived, never a
    /// hand-picked ceiling.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NoSpace`] if every entry is live and the table
    ///   already covers the controller's slot count.
    /// * [`DriverError::LengthOutOfRange`] on bookkeeping-heap exhaustion
    ///   (deterministic OOM).
    fn claim_hub_entry(&mut self) -> Result<usize, DriverError> {
        if let Some(index) = self.hubs.iter().position(Option::is_none) {
            return Ok(index);
        }
        if self.hubs.len() >= usize::from(self.xhci.max_slots()) {
            return Err(DriverError::NoSpace);
        }
        push_free_entry(&mut self.hubs)
    }

    /// Index of the live device the hub at `hub_index` serves on its
    /// downstream `port`, when one is.
    fn device_index_for_hub_and_port(&self, hub_index: usize, port: u8) -> Option<usize> {
        self.devices.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(|device| device.hub_port == port && device.parent_hub == hub_index)
        })
    }

    /// Index of the live child *hub* the hub at `hub_index` carries on its
    /// downstream `port`, when one is.
    fn hub_index_for_hub_and_port(&self, hub_index: usize, port: u8) -> Option<usize> {
        self.hubs.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(|hub| hub.parent == Some(hub_index) && hub.parent_port == port)
        })
    }

    /// Claim a device-table entry and allocate its DMA region: reuse a
    /// free entry (one with no live device *and* no claimed region — a
    /// region kept by a downstream hub's contexts,
    /// [`HubState::device_region`], is not free even though no served
    /// device occupies its entry) or grow the table, then grow a fresh
    /// region chunk for it. The table is bounded by the controller's
    /// reported slot count times the servable interfaces per slot (a
    /// composite device's sibling interfaces share one slot) — a
    /// silicon-derived ceiling, never a hand-picked one.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NoSpace`] if every entry is claimed and the table
    ///   already covers the silicon-derived ceiling.
    /// * [`DriverError::LengthOutOfRange`] on DMA or bookkeeping-heap
    ///   exhaustion (deterministic OOM — the attach fails closed and every
    ///   already-served device keeps its service).
    fn claim_device_entry(&mut self) -> Result<usize, DriverError> {
        let free = (0..self.devices.len())
            .find(|&index| self.devices[index].is_none() && self.regions[index].is_none());
        let index = if let Some(index) = free {
            index
        } else {
            let ceiling = usize::from(self.xhci.max_slots()) * MAX_INTERFACES;
            if self.devices.len() >= ceiling {
                return Err(DriverError::NoSpace);
            }
            let index = push_free_entry(&mut self.devices)?;
            match push_free_entry(&mut self.regions) {
                Ok(region_index) => debug_assert_eq!(region_index, index),
                Err(err) => {
                    // Keep the tables index-aligned on the failed path.
                    self.devices.pop();
                    return Err(err);
                }
            }
            index
        };
        let base = self
            .dma
            .grow(DeviceRegion::layout_len(self.layout.ctx_size))?;
        self.regions[index] = Some(DeviceRegion::at(base, self.layout.ctx_size));
        Ok(index)
    }

    /// The claimed DMA region backing device-table entry `index`.
    ///
    /// # Errors
    ///
    /// [`DriverError::OutOfRange`] if the entry has no claimed region (a
    /// stale or forged index — fail closed).
    fn device_region(&self, index: usize) -> Result<DeviceRegion, DriverError> {
        self.regions
            .get(index)
            .copied()
            .flatten()
            .ok_or(DriverError::OutOfRange)
    }

    /// Release device-table entry `index`'s claimed DMA region, returning
    /// its chunk to the bank. A no-op for an entry with no claimed region.
    fn release_device_region(&mut self, index: usize) {
        if let Some(region) = self.regions.get_mut(index).and_then(Option::take) {
            // The chunk base was minted by `grow`; a release refusal would
            // mean corrupted bookkeeping, and the entry is already logically
            // freed either way — the engine carries no logging seam, and the
            // fail-closed property (a stale offset maps to no chunk) holds
            // regardless.
            let _ = self.dma.release(region.base);
        }
    }

    /// Release every claimed region that no live device occupies, no hub's
    /// contexts own, and the active EP0 cursor does not point into — the
    /// chunks a failed enumeration stranded. Idempotent; called on the
    /// error paths of the attach flows so an aborted attach leaks no DMA.
    fn release_unattached_regions(&mut self) {
        for index in 0..self.regions.len() {
            let Some(region) = self.regions[index] else {
                continue;
            };
            if self.devices[index].is_some() {
                continue;
            }
            let hub_claimed = self.hubs.iter().any(|hub| {
                hub.as_ref()
                    .is_some_and(|hub| hub.device_region == Some(index))
            });
            if hub_claimed || self.ep0_ring_off == region.ep0_ring {
                continue;
            }
            self.release_device_region(index);
        }
    }

    /// Drop every tracked device and hub and release their
    /// demand-allocated chunks, leaving only the shared chunk live — the
    /// common teardown of the full re-enumeration paths (a root-hub
    /// disconnect, or a full controller reset that rebuilds the tree from
    /// scratch).
    fn reset_device_tracking(&mut self) {
        self.slot = 0;
        self.active_device = None;
        self.active_hub = None;
        self.pending_hub_endpoint = None;
        // Rest the EP0 cursor on the root ring first, so no released chunk
        // remains the active control target.
        self.ep0_ring_off = self.layout.ep0_ring;
        self.output_ctx_off = self.layout.output_ctx;
        for index in 0..self.devices.len() {
            self.devices[index] = None;
            self.release_device_region(index);
        }
        self.devices.clear();
        self.regions.clear();
        let hubs = core::mem::take(&mut self.hubs);
        for hub in hubs.into_iter().flatten() {
            let _ = self.dma.release(hub.region.base);
        }
        self.freed_slots.clear();
    }

    /// Whether a served device is live at `index`.
    #[must_use]
    pub fn device_live(&self, index: usize) -> bool {
        self.device(index).is_some()
    }

    /// Number of device-table entries (live or free) — the index bound a
    /// consumer reconciles its per-index state against
    /// ([`Self::device_live`] indices lie below it). Grows as devices
    /// attach and shrinks only on a full re-enumeration.
    #[must_use]
    pub fn device_table_len(&self) -> usize {
        self.devices.len()
    }

    /// Whether any served device is live.
    #[must_use]
    pub fn any_device_live(&self) -> bool {
        self.devices.iter().any(Option::is_some)
    }

    /// Acknowledge the controller interrupter's pending interrupt
    /// (`IMAN.IP`), keeping it armed (xHCI §4.17.5).
    ///
    /// Called at the **start** of servicing a delivered interrupt — before
    /// the reports are drained through [`Self::next_report`] — so a
    /// completion the controller posts during the drain re-asserts `IMAN.IP`
    /// and is not lost. Delegates to [`Xhci::acknowledge_interrupt`], whose
    /// single `USBSTS` read also carries the port-change summary — folded into
    /// [`Self::next_root_change`]'s arming here — and the fault latch, returned
    /// so the caller recovers without a second read of the same register.
    ///
    /// This clears only `IMAN.IP`, never `ERDP`. Event Handler Busy
    /// (`ERDP.EHB`) is released solely by the per-event dequeue advance the
    /// drain performs (`ack_event`, one write per event actually consumed),
    /// so `ERDP` is only ever written with EHB once the controller's event is
    /// genuinely caught up. A standalone `ERDP` write on an empty or
    /// not-yet-consumed ring would tell the controller the ring is drained to
    /// a point behind its own enqueue and re-assert the interrupt
    /// immediately — a self-sustaining storm (the metal symptom: the loop
    /// wakes continuously the moment a key is pressed). So the drain, not a
    /// separate write, owns EHB.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if the register window rejects the access.
    pub fn acknowledge_interrupt(&mut self) -> Result<ControllerStatus, DriverError> {
        let status = self.xhci.acknowledge_interrupt()?;
        if status.port_change {
            self.root_change_pending = true;
        }
        Ok(status)
    }

    /// Device-visible address of the bank's virtual offset `offset`.
    ///
    /// # Errors
    ///
    /// [`DriverError::OutOfRange`] if `offset` lies in no live chunk (a
    /// stale offset kept past its chunk's release — fail closed).
    fn phys_of(&self, offset: usize) -> Result<u64, DriverError> {
        self.dma.phys_of(offset)
    }

    /// Consume the next controller event, advancing `ERDP` when one
    /// was taken.
    fn poll_event(&mut self) -> Result<Option<Trb>, DriverError> {
        // First snapshot: decide *whether* the controller has produced the
        // event at the dequeue point, by its cycle bit alone.
        let trb = self.read_event_slot()?;
        if !self.event_cursor.owned(trb) {
            return Ok(None);
        }
        // An event is owned. The controller writes the entry body before it
        // sets the cycle bit; on the device-shared Normal-Non-Cacheable DMA
        // region those two writes are not ordered for this PE without a
        // barrier, so the first snapshot's body bytes may predate the cycle
        // bit (a torn read pairing a fresh cycle with a stale TRB pointer —
        // the metal `REJECT_ADDRESS_MISMATCH` this fixes). Order the body read
        // after the cycle observation, then re-read and consume (see `tairix_dma_barrier`).
        tairix_dma_barrier::dma_rmb();
        let trb = self.read_event_slot()?;
        // Re-confirm ownership on the post-barrier snapshot, then verify the
        // entry has actually landed before consuming it. The read barrier
        // orders *this PE's* reads (body after cycle), but it cannot order the
        // *controller's* writes into RAM: on the BCM2711 PCIe path the VL805's
        // 16-byte TRB write is not guaranteed to reach RAM atomically, so the
        // announcing cycle bit can become visible while the body is still the
        // zeroed initial state. A real event TRB never has type 0, so a
        // cycle-owned entry whose type is still 0 has not fully landed: leave it
        // un-consumed (do not advance the cursor, do not write `ERDP`) and
        // re-read it on the next wake once the body is visible. Consuming such a
        // phantom would advance the dequeue past the controller's enqueue and
        // permanently desynchronise the consumer cycle, wedging the interrupter
        // with Event Handler Busy stuck set so no further completion interrupts
        // — the metal "first key then silent" fault.
        if !self.event_cursor.owned(trb) {
            return Ok(None);
        }
        if trb.trb_type_raw() == 0 {
            return Ok(None);
        }
        let Some(event) = self.event_cursor.pop(trb) else {
            return Ok(None);
        };
        // A Port Status Change Event is the controller's notification that some
        // root port's change bits latched. Recording it here — at the one point
        // every ring consumer funnels through — is what lets the root-port scan
        // be event-driven rather than run on every interrupt: no consumer can
        // drain such an event without arming the scan.
        if event.trb_type() == Ok(TrbType::PortStatusChange) {
            self.root_change_pending = true;
        }
        let erdp = self.phys_of(self.layout.event_segment)?
            + (self.event_cursor.dequeue_index() * trb::TRB_LEN) as u64;
        self.xhci.ack_event(erdp)?;
        Ok(Some(event))
    }

    /// Read the one event-ring entry at the cursor's dequeue point out of DMA.
    ///
    /// Only that entry decides whether an event is pending, so reading it alone
    /// keeps a poll at 16 bytes of non-cacheable traffic instead of the whole
    /// segment.
    fn read_event_slot(&mut self) -> Result<Trb, DriverError> {
        let mut image = [0u8; trb::TRB_LEN];
        self.dma.read(
            self.layout.event_segment + self.event_cursor.dequeue_offset(),
            &mut image,
        )?;
        Ok(Trb::from_bytes(image))
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

    /// Index of the served device whose interrupt-IN report completion
    /// `event` is, routed by its stable slot and endpoint.
    fn report_async_index(&self, event: Trb) -> Option<usize> {
        self.devices.iter().position(|entry| {
            entry.as_ref().is_some_and(|device| {
                device.int_dci != DCI_CONTROL
                    && event.slot_id() == device.slot
                    && event.endpoint_id() == device.int_dci
            })
        })
    }

    /// Index of the addressed hub whose status-change endpoint completion
    /// `event` is, routed by its stable slot and endpoint.
    fn hub_async_index(&self, event: Trb) -> Option<usize> {
        self.hubs.iter().position(|entry| {
            entry.as_ref().is_some_and(|hub| {
                hub.int_dci != 0
                    && event.slot_id() == hub.slot
                    && event.endpoint_id() == hub.int_dci
            })
        })
    }

    /// Whether `event` is a trailing transfer completion the controller posted
    /// for a just-freed device slot ([`Self::freed_slots`]).
    ///
    /// A physical unplug can drop an in-flight transfer, and tearing the slot
    /// down (Disable Slot) can itself leave a completion event behind; either
    /// lands on the shared event ring *after* the device endpoint is gone, so
    /// it matches neither [`Self::report_async_index`] (the device entry is
    /// cleared) nor [`Self::hub_async_index`]. Recognising it here lets the
    /// event-ring consumers drain it instead of faulting — a fatal fault there
    /// would silence the hub status-change watch and a later re-plug would go
    /// unseen.
    fn is_stale_freed_transfer(&self, event: Trb) -> bool {
        event.slot_id() != 0 && self.freed_slots.contains(&event.slot_id())
    }

    /// Index of the served device on whose configured bulk endpoint `event`
    /// completed, routed by its stable slot and endpoint.
    fn bulk_async_index(&self, event: Trb) -> Option<usize> {
        self.devices.iter().position(|entry| {
            entry.as_ref().is_some_and(|device| {
                event.slot_id() == device.slot
                    && device.bulk_pipe_of_dci(event.endpoint_id()).is_some()
            })
        })
    }

    /// Dispatch an asynchronous completion `event` sharing the one event ring
    /// to its endpoint's consumer, so a synchronous EP0/command wait — or the
    /// shared report drain — neither faults on it nor drops it.
    ///
    /// A device report completion is **captured** into that device's report
    /// FIFO ([`Self::capture_report_event`]: decode, retire, re-arm); a hub
    /// status-change completion is parked for the hub watcher; a bulk
    /// completion is parked in the device's bulk FIFO. Several interrupt
    /// transfers are armed at once ([`INT_ARM_DEPTH`]), so more than one report
    /// completion can arrive before it is drained — the report FIFO holds
    /// them, unlike the hub's single status slot (only one status transfer is
    /// ever armed).
    ///
    /// Returns `Ok(true)` when `event` belonged to a registered async endpoint
    /// (report, hub status-change, bulk) or was a tolerated freed-slot
    /// completion, `Ok(false)` when it belonged to none (the caller decides
    /// whether that is a fault). A hub double-completion still fails closed
    /// ([`DriverError::DeviceFault`]); a report capture fault is recorded on
    /// the device rather than propagated here.
    fn stash_async_event(&mut self, event: Trb) -> Result<bool, DriverError> {
        if let Some(index) = self.report_async_index(event) {
            // Capture the report into the device's FIFO (decode, retire the
            // ring slot, re-arm). A capture-side fault is recorded on the
            // device for its next URB to surface, never propagated here: an
            // asynchronous report must not fault the synchronous EP0/command
            // wait that happened to observe it, nor the shared drain.
            let _ = self.capture_report_event(index, event);
            return Ok(true);
        }
        if let Some(hub_index) = self.hub_async_index(event) {
            if let Some(hub) = self.hubs[hub_index].as_mut() {
                if hub.pending.is_some() {
                    return Err(DriverError::DeviceFault);
                }
                hub.pending = Some(event);
                return Ok(true);
            }
        }
        if let Some(index) = self.bulk_async_index(event) {
            if let Some(device) = self.devices[index].as_mut() {
                // Several bulk TDs can be outstanding at once, so bulk parks
                // in a FIFO sized to both rings' in-flight bound; overflow
                // means the controller posted more completions than TDs were
                // queued — a protocol violation, surfaced by the push.
                device.pending_bulk.push(event)?;
                return Ok(true);
            }
        }
        if self.is_stale_freed_transfer(event) {
            return Ok(true);
        }
        Ok(false)
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
        let deadline = self.wait.now_us().saturating_add(AWAIT_EVENT_BUDGET_US);
        loop {
            let Some(event) = self.poll_event()? else {
                let now = self.wait.now_us();
                if now >= deadline {
                    self.last_reject = REJECT_BUDGET_TIMEOUT;
                    return Err(DriverError::DeviceFault);
                }
                // Park until the controller's interrupt or the remaining
                // budget, never spinning the event ring; a spurious wake
                // re-polls against the same deadline.
                self.wait.wait_us(deadline - now);
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
                        // The event is not for the transfer/command this
                        // synchronous wait issued. If it is an asynchronous
                        // interrupt-IN completion for a registered endpoint
                        // (the device's report endpoint, or the hub's
                        // status-change endpoint), park it for that endpoint's
                        // consumer and keep waiting — the shared event ring
                        // multiplexes all endpoints, so an in-flight hub
                        // status report or a stray keystroke completion must
                        // not fault an EP0 transfer. Anything else is a
                        // genuine controller fault.
                        if self.stash_async_event(event)? {
                            continue;
                        }
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
        self.control_transfer(setup, data_in_len, None)
    }

    /// Run one control-OUT transfer on the default endpoint: `setup`, an
    /// OUT data stage carrying `data` (staged through the control data
    /// buffer), and the status stage.
    fn control_out_transfer(&mut self, setup: [u8; 8], data: &[u8]) -> Result<(), DriverError> {
        self.control_transfer(setup, 0, Some(data)).map(|_| ())
    }

    /// The shared control-transfer stage builder behind [`Self::control`]
    /// and [`Self::control_out_transfer`]: SETUP, an optional data stage —
    /// IN of `data_in_len` bytes into the control data buffer, or OUT
    /// carrying `out_data` (`data_in_len` must then be `0`) — and the
    /// status stage, which runs opposite to the data direction (IN when
    /// there is no data stage, §4.11.2.2). Returns the bytes the device
    /// actually moved in the data stage.
    ///
    /// A device STALL surfaces as [`DriverError::EndpointStalled`] with the
    /// control endpoint already recovered
    /// ([`Self::recover_control_endpoint`]), so the caller may issue fresh
    /// control transfers immediately.
    fn control_transfer(
        &mut self,
        setup: [u8; 8],
        data_in_len: u32,
        out_data: Option<&[u8]>,
    ) -> Result<u32, DriverError> {
        let data_len = match out_data {
            Some(data) => {
                if data_in_len != 0 {
                    return Err(DriverError::LengthOutOfRange);
                }
                u32::try_from(data.len()).map_err(|_| DriverError::LengthOutOfRange)?
            }
            None => data_in_len,
        };
        if data_len as usize > CTRL_DATA_LEN {
            return Err(DriverError::LengthOutOfRange);
        }
        // Stage the OUT payload into the control data buffer before any
        // TRB is published, so a refused write leaves nothing armed.
        if let Some(data) = out_data {
            if !data.is_empty() {
                self.dma.write(self.layout.ctrl_data, data)?;
            }
        }
        self.reset_event_diagnostics();
        let transfer_type = if data_len == 0 {
            trb::SETUP_TRT_NO_DATA
        } else if out_data.is_some() {
            trb::SETUP_TRT_OUT
        } else {
            trb::SETUP_TRT_IN
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
        if data_len > 0 {
            // An IN data stage interrupts on a short packet so the honest
            // byte count is read from the residual; an OUT stage moves
            // host bytes and carries no direction flag.
            let data_flags = if out_data.is_some() {
                0
            } else {
                trb::CONTROL_DIR_IN | trb::CONTROL_ISP
            };
            let data_trb = Trb::new(
                TrbType::DataStage,
                self.phys_of(self.layout.ctrl_data)?,
                data_len,
                data_flags,
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
        let status_direction = if data_len > 0 && out_data.is_none() {
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
        self.complete_control_transfer(data_address, status.address, data_len)
    }

    /// Await the pushed control transfer's completion: at most two events
    /// arrive — a short-packet event for the data stage, then the
    /// status-stage completion — and the honest data-stage byte count is
    /// `data_len` minus the reported residual.
    fn complete_control_transfer(
        &mut self,
        data_address: Option<u64>,
        status_address: u64,
        data_len: u32,
    ) -> Result<u32, DriverError> {
        let mut residual = 0;
        for _ in 0..2 {
            let watch = [data_address.unwrap_or(status_address), status_address];
            let event = self.await_event_for(&watch)?;
            if event.trb_type() != Ok(TrbType::TransferEvent)
                || event.slot_id() != self.slot
                || event.endpoint_id() != DCI_CONTROL
            {
                return Err(DriverError::DeviceFault);
            }
            match event.completion_code() {
                Ok(CompletionCode::Success | CompletionCode::ShortPacket) => {}
                // A protocol STALL: the device refused the request. The
                // controller halts the control endpoint (xHCI §4.8.3);
                // recover it in place — the device side self-clears at the
                // next SETUP (USB 2.0 §8.5.3.4) — and surface the refusal
                // distinctly so a class driver can treat it as an answer.
                Ok(CompletionCode::StallError) => {
                    self.recover_control_endpoint()?;
                    return Err(DriverError::EndpointStalled);
                }
                _ => return Err(DriverError::DeviceFault),
            }
            if data_address == Some(event.parameter) {
                residual = event.transfer_residual();
                continue;
            }
            while self.ep0_ring.in_flight() > 0 {
                self.ep0_ring.retire_one()?;
            }
            return data_len
                .checked_sub(residual)
                .ok_or(DriverError::DeviceFault);
        }
        Err(DriverError::DeviceFault)
    }

    /// Recover the **active** default control endpoint after a device
    /// STALL: drop the abandoned stage TRBs, Reset Endpoint (§4.6.8) to
    /// clear the controller-side halt, rebuild the EP0 transfer ring at its
    /// base, and repoint the controller's dequeue there (§4.6.10). No
    /// device-side `CLEAR_FEATURE` is needed: a control endpoint's protocol
    /// STALL ends at the next SETUP (USB 2.0 §8.5.3.4).
    fn recover_control_endpoint(&mut self) -> Result<(), DriverError> {
        // The recovery's own successful commands must not overwrite the
        // observed STALL: the diagnostic (`last_completion_code`) preserves
        // the code the failing transfer saw.
        let observed_completion = self.last_completion;
        // The halt abandoned every stage TRB still in flight; drop them
        // from the software ring (they are answered by the STALL itself).
        while self.ep0_ring.in_flight() > 0 {
            self.ep0_ring.retire_one()?;
        }
        self.command(Trb::new(
            TrbType::ResetEndpoint,
            0,
            0,
            trb::control_slot(self.slot) | trb::control_endpoint(DCI_CONTROL),
        ))?;
        // Rebuild the ring at its base with a fresh cycle and point the
        // controller's dequeue at it (Dequeue Cycle State 1 to match).
        let zeros = [0u8; trb::TRB_LEN];
        for ring_slot in 0..RING_TRBS {
            self.dma
                .write(self.ep0_ring_off + ring_slot * trb::TRB_LEN, &zeros)?;
        }
        let base = self.phys_of(self.ep0_ring_off)?;
        let (ring, link) = ProducerRing::new(RING_TRBS, base)?;
        self.dma.write(
            self.ep0_ring_off + ring.link_slot() * trb::TRB_LEN,
            &link.to_bytes(),
        )?;
        self.ep0_ring = ring;
        self.command(Trb::new(
            TrbType::SetTrDequeuePointer,
            base | 1,
            0,
            trb::control_slot(self.slot) | trb::control_endpoint(DCI_CONTROL),
        ))?;
        self.last_completion = observed_completion;
        Ok(())
    }

    /// Run an *optional* control request (no data stage), tolerating a
    /// protocol STALL.
    ///
    /// A device that does not implement an optional class request (e.g.
    /// `SET_PROTOCOL`, mandatory only for boot-subclass devices) STALLs it;
    /// the control endpoint is recovered by [`Self::control_transfer`] and
    /// the refusal absorbed rather than aborting an otherwise-enumerable
    /// keyboard. Every other failure still fails closed; the raw code is
    /// preserved in [`Self::last_completion_code`].
    fn control_optional(&mut self, setup: [u8; 8]) -> Result<(), DriverError> {
        match self.control(setup, 0) {
            Ok(_) | Err(DriverError::EndpointStalled) => Ok(()),
            Err(other) => Err(other),
        }
    }

    /// Prime one interrupt-IN transfer on device `index`'s endpoint: a
    /// Normal TRB pointing at the report buffer paired with the ring slot
    /// it lands in.
    fn arm_report(&mut self, index: usize) -> Result<(), DriverError> {
        let region = self.device(index).ok_or(DriverError::NotFound)?.region;
        let bufs_phys = self.phys_of(region.report_bufs)?;
        let device = self.device_mut(index).ok_or(DriverError::NotFound)?;
        // Arm the transfer to the endpoint's own packet size, never the full
        // capture buffer: a full/low-speed endpoint's periodic split through a
        // high-speed hub faults (Split Transaction Error) when asked for more
        // than the per-interval budget the transaction translator scheduled.
        let capture_len = device.int_capture_len();
        let ring = device.int_ring.as_mut().ok_or(DriverError::DeviceFault)?;
        let slot = ring.enqueue_slot();
        let buffer = bufs_phys + (slot * CAPTURE_LEN) as u64;
        let report_len = u32::try_from(capture_len).map_err(|_| DriverError::LengthOutOfRange)?;
        let normal = Trb::new(
            TrbType::Normal,
            buffer,
            report_len,
            trb::CONTROL_IOC | trb::CONTROL_ISP,
        );
        let outcome = ring.push(normal)?;
        let link_slot = ring.link_slot();
        publish(&mut self.dma, region.int_ring, link_slot, &outcome)
    }

    /// Keep device `index`'s interrupt-IN endpoint armed with up to
    /// [`INT_ARM_DEPTH`] outstanding transfers, arming as many fresh ones as
    /// the current in-flight count is short and ringing the doorbell once if
    /// any were armed.
    ///
    /// This is what keeps the report endpoint continuously live: with a
    /// transfer always armed the controller has somewhere to write each
    /// interval's report, so a report is not dropped merely because software
    /// was slow to run. Combined with the per-interrupt drain into the report
    /// FIFO ([`Self::pump_reports`]), device polling is decoupled from the
    /// class driver's consume rate (the metal "missed keypresses under load"
    /// defect). In-flight transfers the controller has already consumed but
    /// whose completion this engine has not yet retired still count against
    /// the depth, so this never over-fills the ring; the count is topped back
    /// up as [`Self::capture_report_event`] retires each completion.
    ///
    /// # Errors
    ///
    /// [`DriverError::NotFound`] / [`DriverError::DeviceFault`] if the device
    /// or its interrupt ring is gone, or a controller fault while arming.
    fn ensure_reports_armed(&mut self, index: usize) -> Result<(), DriverError> {
        let (device_slot, int_dci) = {
            let device = self.device(index).ok_or(DriverError::NotFound)?;
            (device.slot, device.int_dci)
        };
        let mut armed_any = false;
        loop {
            let in_flight = self
                .device(index)
                .and_then(|device| device.int_ring.as_ref().map(ProducerRing::in_flight))
                .ok_or(DriverError::DeviceFault)?;
            if in_flight >= INT_ARM_DEPTH {
                break;
            }
            match self.arm_report(index) {
                Ok(()) => armed_any = true,
                // The ring cannot hold another transfer: the depth already
                // armed is the most this ring supports, which is enough.
                Err(DriverError::Busy) => break,
                Err(err) => return Err(err),
            }
        }
        if armed_any {
            self.xhci.ring_doorbell(device_slot, u32::from(int_dci))?;
        }
        Ok(())
    }

    /// Address the device in `slot` (§4.3.4): program the input control
    /// context (A0 | A1), the slot context from `base` (speed, root-hub
    /// port, and — for a downstream device — Route String and TT) and the
    /// EP0 context, point the DCBAA at the active output context, then
    /// issue Address Device. The EP0 context points at the active EP0 ring
    /// ([`Self::ep0_ring_off`]), so a downstream device addressed after
    /// [`Self::rebind_to_device_region`] gets its own ring.
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
                self.phys_of(self.ep0_ring_off)?,
            ),
        )?;
        let output_ctx = self.phys_of(self.output_ctx_off)?;
        self.dma.write(
            self.layout.dcbaa + usize::from(slot) * 8,
            &output_ctx.to_le_bytes(),
        )?;
        self.stage = EnumStage::AddressDevice;
        self.command(Trb::new(
            TrbType::AddressDevice,
            self.phys_of(self.layout.input_ctx)?,
            0,
            trb::control_slot(slot),
        ))?;
        Ok(())
    }

    /// Re-evaluate the default control endpoint's Max Packet Size to the
    /// device-reported `bMaxPacketSize0` (§4.6.7): Address Device assumed
    /// the speed's worst case, and with an overstated context every EP0 IN
    /// transfer longer than one device packet terminates short at the
    /// first packet — the metal fault a full-speed wireless receiver with
    /// an 8-byte EP0 hits on the 18-byte descriptor read. The input
    /// context names only the EP0 context (A1); the controller evaluates
    /// just its Max Packet Size field (§6.2.3.3), the ring fields carried
    /// for well-formedness only.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the controller rejects the
    ///   command.
    fn evaluate_ep0_max_packet(&mut self, slot: u8, max_packet: u32) -> Result<(), DriverError> {
        self.write_input_ctx(0, &input_control_dwords(0b10))?;
        self.write_input_ctx(
            1 + usize::from(DCI_CONTROL),
            &ep_ctx_dwords(
                EP_TYPE_CONTROL,
                max_packet,
                0,
                self.phys_of(self.ep0_ring_off)?,
            ),
        )?;
        self.command(Trb::new(
            TrbType::EvaluateContext,
            self.phys_of(self.layout.input_ctx)?,
            0,
            trb::control_slot(slot),
        ))?;
        Ok(())
    }

    /// Read and decode the 18-byte device descriptor in two steps (USB 2.0
    /// §5.5.3): the Address Device EP0 context assumed the speed's
    /// worst-case packet size, but a full-speed device may legally use
    /// 8/16/32 — and with an overstated context, any EP0 IN transfer longer
    /// than one device packet terminates short at the first packet. So one
    /// worst-case-safe packet ending at `bMaxPacketSize0` is read first,
    /// the context is re-evaluated to the honest size
    /// ([`Self::evaluate_ep0_max_packet`]), and only then the full
    /// descriptor.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] for a forged descriptor prefix or a
    ///   `bMaxPacketSize0` the speed does not permit.
    /// * [`DriverError::DeviceFault`] for any controller/device failure.
    fn read_device_descriptor(
        &mut self,
        slot: u8,
        base: SlotCtxBase,
    ) -> Result<DeviceDescriptor, DriverError> {
        self.stage = EnumStage::GetDeviceDescriptor;
        let prefix_len = u32::try_from(DEVICE_DESCRIPTOR_PREFIX_LEN)
            .map_err(|_| DriverError::LengthOutOfRange)?;
        let prefix_len_u16 = u16::try_from(DEVICE_DESCRIPTOR_PREFIX_LEN)
            .map_err(|_| DriverError::LengthOutOfRange)?;
        let transferred = self.control(setup_get_device_descriptor(prefix_len_u16), prefix_len)?;
        if transferred != prefix_len {
            return Err(DriverError::DeviceFault);
        }
        let mut prefix = [0u8; DEVICE_DESCRIPTOR_PREFIX_LEN];
        self.dma.read(self.layout.ctrl_data, &mut prefix)?;
        if usize::from(prefix[0]) < DeviceDescriptor::LEN || prefix[1] != 0x01 {
            return Err(DriverError::BadMagic);
        }
        let ep0_max = ep0_max_packet_from_descriptor(base.speed, prefix[7])?;
        if ep0_max != ep0_max_packet(base.speed)? {
            self.evaluate_ep0_max_packet(slot, ep0_max)?;
        }

        let descriptor_len =
            u32::try_from(DeviceDescriptor::LEN).map_err(|_| DriverError::LengthOutOfRange)?;
        let descriptor_len_u16 =
            u16::try_from(DeviceDescriptor::LEN).map_err(|_| DriverError::LengthOutOfRange)?;
        let transferred = self.control(
            setup_get_device_descriptor(descriptor_len_u16),
            descriptor_len,
        )?;
        if transferred != descriptor_len {
            return Err(DriverError::DeviceFault);
        }
        let mut bytes = [0u8; DeviceDescriptor::LEN];
        self.dma.read(self.layout.ctrl_data, &mut bytes)?;
        DeviceDescriptor::decode(&bytes)
    }

    /// Read the configuration descriptor at its exact advertised length
    /// into `config_bytes`, returning the byte count to decode: the 9-byte
    /// header first for `wTotalLength`, then precisely that many bytes
    /// (clamped to [`CTRL_DATA_LEN`] — a validation bound on
    /// device-supplied data, not a scalable capacity). Asking for more
    /// than the device holds relies on it short-packeting the reply —
    /// conforming devices do, but real receivers have been caught
    /// mishandling an over-long request, so only bytes the device
    /// advertised are ever requested.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] for a non-configuration header or an
    ///   impossible `wTotalLength`.
    /// * [`DriverError::DeviceFault`] for any controller/device failure.
    fn read_configuration(
        &mut self,
        config_bytes: &mut [u8; CTRL_DATA_LEN],
    ) -> Result<usize, DriverError> {
        self.stage = EnumStage::GetConfigDescriptor;
        let header_len = u32::try_from(InterfaceInfo::CONFIG_HEADER_LEN)
            .map_err(|_| DriverError::LengthOutOfRange)?;
        let header_len_u16 = u16::try_from(InterfaceInfo::CONFIG_HEADER_LEN)
            .map_err(|_| DriverError::LengthOutOfRange)?;
        let transferred = self.control(
            setup_get_configuration_descriptor(header_len_u16),
            header_len,
        )?;
        if transferred != header_len {
            return Err(DriverError::DeviceFault);
        }
        let mut header = [0u8; InterfaceInfo::CONFIG_HEADER_LEN];
        self.dma.read(self.layout.ctrl_data, &mut header)?;
        if header[1] != DESC_TYPE_CONFIGURATION {
            return Err(DriverError::BadMagic);
        }
        let total = usize::from(u16::from_le_bytes([header[2], header[3]]));
        if total < InterfaceInfo::CONFIG_HEADER_LEN {
            return Err(DriverError::BadMagic);
        }
        let total = usize::min(total, CTRL_DATA_LEN);
        let total_u16 = u16::try_from(total).map_err(|_| DriverError::LengthOutOfRange)?;
        let total_u32 = u32::try_from(total).map_err(|_| DriverError::LengthOutOfRange)?;
        let transferred = self.control(setup_get_configuration_descriptor(total_u16), total_u32)?;
        if transferred != total_u32 {
            return Err(DriverError::DeviceFault);
        }
        self.dma.read(self.layout.ctrl_data, config_bytes)?;
        Ok(total)
    }

    /// Complete enumeration of the device already Enable-Slotted into
    /// `slot` and Address-Deviced with topology `base`: read its device
    /// and configuration descriptors and, for each servable interface,
    /// configure its endpoints, then `SET_CONFIGURATION` and a best-effort
    /// `SET_PROTOCOL(boot)` per HID interface.
    ///
    /// Shared by the root ([`Self::attach_root_port`]) and downstream
    /// ([`Self::attach_downstream_device`]) paths so the post-Address
    /// sequence is written once; they differ only in the topology carried
    /// in `base`. The interrupt-IN endpoint is armed and doorbelled
    /// **only** for a HID interface: arming a hub's status-change endpoint
    /// here would deliver asynchronous reports that interleave with the
    /// EP0 hub-class `GET_STATUS` transfers and wedge the control ring, so
    /// it is captured for the hub-install path instead; `SET_PROTOCOL
    /// (boot)` is likewise HID-only, since a non-HID interface STALLs it
    /// and halts the control endpoint.
    ///
    /// The first served interface creates the device-table entry at
    /// `index` — its endpoint rings live in that index's layout region —
    /// and leaves it the active control context. Each **further** served
    /// interface of a composite device (a wireless keyboard+mouse receiver)
    /// takes its own free table index and ring region while sharing the
    /// device's slot and EP0, so each function is served — and published —
    /// separately. `hub_port` records the hub downstream port the device
    /// hangs off (`0` for a root-attached device) and `parent_hub` the
    /// hub-table index of that hub. A hub, or an interface this engine
    /// serves no transfer type for, creates no entry.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] if a descriptor is forged.
    /// * [`DriverError::DeviceFault`] for any controller/device failure.
    fn finish_enumeration(
        &mut self,
        slot: u8,
        base: SlotCtxBase,
        index: usize,
        hub_port: u8,
        parent_hub: usize,
    ) -> Result<DeviceDescriptor, DriverError> {
        let descriptor = self.read_device_descriptor(slot, base)?;
        let mut config_bytes = [0u8; CTRL_DATA_LEN];
        let total = self.read_configuration(&mut config_bytes)?;
        let interfaces = InterfaceInfo::decode_all(&config_bytes[..total])?;
        let first = interfaces[0].ok_or(DriverError::BadMagic)?;

        // A hub's interrupt-IN status-change endpoint is captured (not armed
        // here) so the hub-install path can configure and watch it once the
        // slot is marked a hub; arming it inline would interleave async
        // status reports with the EP0 hub-class transfers that follow.
        if descriptor.is_hub() && first.int_dci != DCI_CONTROL {
            self.pending_hub_endpoint = Some((
                first.int_dci,
                u32::from(first.int_max_packet),
                interrupt_interval(base.speed, first.int_b_interval),
            ));
        }

        if self.devices.get(index).is_none_or(Option::is_some) {
            // The entry must be free: overwriting a live device would leak
            // its slot and rings.
            return Err(DriverError::Busy);
        }

        let plan = self.plan_interfaces(index, &interfaces);

        // Every Configure Endpoint rewrites the slot context, so its
        // Context Entries field must cover the highest DCI any served
        // sibling interface uses — a later sibling's command must never
        // shrink an already-configured endpoint out of scope (xHCI §6.2.2).
        let mut max_dci = DCI_CONTROL;
        for (_, iface) in plan.iter().flatten() {
            max_dci = max_dci
                .max(iface.int_dci)
                .max(iface.bulk_in_dci)
                .max(iface.bulk_out_dci)
                .max(iface.bulk_in2_dci)
                .max(iface.bulk_out2_dci);
        }

        let mut rings: [Option<ConfiguredRings>; MAX_INTERFACES] = [const { None }; MAX_INTERFACES];
        for (entry, rings_slot) in plan.iter().zip(rings.iter_mut()) {
            let Some((target, iface)) = entry else {
                continue;
            };
            *rings_slot = Some(self.configure_interface(slot, base, *target, iface, max_dci)?);
        }

        self.stage = EnumStage::SetConfiguration;
        self.control(setup_set_configuration(first.configuration_value), 0)?;

        // Bring each HID interface up in **report protocol** where its Report
        // Descriptor can be parsed: report protocol is what lets `SET_IDLE`
        // quiesce an idle device, so it reports only on change instead of
        // streaming a duplicate every polling interval (some devices honour
        // `SET_IDLE` only in report protocol — the on-metal mouse whose idle
        // reports pegged a core). An interface whose descriptor cannot be read
        // or parsed falls back to boot protocol, unchanged. The parsed map is
        // kept per plan slot so the installed device normalises its
        // report-protocol reports back into the boot layout the class drivers
        // consume, and its descriptor beside it for the metal diagnostic.
        let mut setups: [Option<HidSetup>; MAX_INTERFACES] = [const { None }; MAX_INTERFACES];
        for (slot_pos, entry) in plan.iter().enumerate() {
            if let Some((_, iface)) = entry {
                if iface.is_hid() {
                    setups[slot_pos] = Some(self.configure_hid_protocol(iface)?);
                }
            }
        }

        let mut installed = false;
        for (slot_pos, (entry, rings_slot)) in plan.iter().zip(rings.iter_mut()).enumerate() {
            let (Some((target, iface)), Some(configured)) = (entry, rings_slot.take()) else {
                continue;
            };
            let region = self.device_region(*target)?;
            // The interrupt DCI is live exactly when its ring was
            // configured: a HID report endpoint, or a bulk interface's CBI
            // completion endpoint.
            let int_dci = if configured.int_ring.is_some() {
                iface.int_dci
            } else {
                DCI_CONTROL
            };
            self.install_device_entry(
                *target,
                slot,
                hub_port,
                base.root_port,
                parent_hub,
                region,
                descriptor,
                iface,
                int_dci,
                configured.int_ring,
                setups[slot_pos].take().unwrap_or_else(HidSetup::none),
                configured.bulk_rings,
            );
            installed = true;
        }
        if installed {
            // The primary entry owns the slot's (currently active) EP0
            // cursor; sibling entries share the slot and route their control
            // transfers through it. A fresh device now owns its slot, so
            // stop tolerating trailing events for previously-freed ones (any
            // such completion has long since arrived in the detach→attach
            // window).
            self.active_device = Some(index);
            self.freed_slots.clear();
        }
        self.stage = EnumStage::Configured;
        Ok(descriptor)
    }

    /// Bring one HID `interface` up in the protocol it needs, returning the
    /// parsed [`tairix_hid::HidReportMap`] used to normalise a report-protocol
    /// interface's reports back into the boot layout, or `None` when the
    /// interface runs in **boot protocol** (the report is already in the boot
    /// layout).
    ///
    /// Report protocol is used only where a device needs it to fall silent at
    /// idle. The on-metal mouse streams a duplicate report every polling
    /// interval in **boot** protocol — it ignores `SET_IDLE` there and only
    /// honours it in report protocol — so a pointer runs in report protocol,
    /// its report-descriptor layout parsed back into the boot layout the class
    /// driver consumes. A **keyboard** needs none of that: it reports only on
    /// a key-state change (so it never causes the idle interrupt storm a
    /// report-streaming mouse does) and stays in the standardised boot
    /// protocol — the fixed 8-byte report the boot decoder is built for —
    /// rather than a report-descriptor parse a composite keyboard's multiple
    /// Report IDs (or an NKRO layout) can misread and silently drop (the
    /// on-metal keyboard whose native report arrived under a Report ID our
    /// parser had not pinned to, dropping every keypress).
    ///
    /// The descriptor is read for **every** HID interface, a keyboard's
    /// included, because it is the only evidence of what an interface really
    /// is and a metal capture is undiagnosable without it. Reading it does not
    /// decide the protocol: a keyboard runs boot protocol whatever its
    /// descriptor says, so for a keyboard the read is purely observational and
    /// a fault reading it cannot fail the bring-up. A pointer's read *is* load
    /// bearing — it selects report protocol — so there a non-STALL fault still
    /// fails closed.
    ///
    /// In every case `SET_IDLE(indefinite)` is issued so an idle device
    /// reports only when its report data changes. All class requests are
    /// optional (a device that STALLs one simply keeps its current mode);
    /// only a non-STALL fault fails the enumeration.
    ///
    /// # Errors
    ///
    /// A non-STALL controller/device fault from a class request or the
    /// descriptor read.
    fn configure_hid_protocol(
        &mut self,
        interface: &InterfaceInfo,
    ) -> Result<HidSetup, DriverError> {
        let keyboard = interface.is_keyboard();
        let descriptor = match self.read_report_descriptor(interface) {
            Ok(descriptor) => descriptor,
            // A keyboard's descriptor decides nothing, so a diagnostic read of
            // it must never cost a bring-up that would otherwise succeed — the
            // device would be left with no keyboard at all. A real controller
            // fault is not hidden: it surfaces on the SET_PROTOCOL/SET_IDLE
            // transfers immediately below.
            Err(_) if keyboard => ReportDescriptor::none(),
            Err(fault) => return Err(fault),
        };
        let map = if keyboard {
            None
        } else {
            tairix_hid::parse_report_descriptor(descriptor.as_bytes())
        };
        self.stage = EnumStage::SetProtocol;
        let protocol = if map.is_some() {
            setup_set_protocol_report(interface.interface_number)
        } else {
            setup_set_protocol_boot(interface.interface_number)
        };
        self.control_optional(protocol)?;
        // Read back before `SET_IDLE` so a healthy enumeration still closes on
        // that request's Success: this read is optional and a device that
        // STALLs it must not leave a stall as the last completion the
        // fault-localising diagnostic reports.
        // Boot decoding is only safe if the device really is in boot protocol,
        // and `SET_PROTOCOL` is optional — a device may accept it and ignore
        // it. Where the answer says the device stayed in report protocol, its
        // reports are ID-prefixed, so the boot decoder would read each leading
        // Report ID byte as the modifier byte and invent held modifiers and
        // keys from sibling collections. Normalise through the parsed map
        // instead, and where there is no map deliver nothing rather than
        // fabricated input.
        let decode = match map {
            Some(map) => ReportDecode::Mapped(map),
            None if self.in_boot_protocol(interface) => ReportDecode::Boot,
            None => match tairix_hid::parse_report_descriptor(descriptor.as_bytes()) {
                Some(map) => ReportDecode::Mapped(map),
                None => ReportDecode::Refused,
            },
        };
        self.stage = EnumStage::SetIdle;
        self.control_optional(setup_set_idle_indefinite(interface.interface_number))?;
        Ok(HidSetup { decode, descriptor })
    }

    /// Whether `interface` reports itself in boot protocol, so the fixed boot
    /// report layout is what its reports actually carry.
    ///
    /// A device that STALLs `GET_PROTOCOL` (the request is optional) cannot
    /// answer, and one whose answer will not read is no better: both are
    /// treated as boot protocol, which is what was asked for and is the mode
    /// every HID boot device must implement. Only an explicit "report
    /// protocol" answer contradicts the request.
    fn in_boot_protocol(&mut self, interface: &InterfaceInfo) -> bool {
        self.stage = EnumStage::GetProtocol;
        let setup = setup_get_protocol(interface.interface_number);
        let Ok(transferred) = self.control(setup, 1) else {
            return true;
        };
        if transferred == 0 {
            return true;
        }
        let mut answer = [0u8; 1];
        if self.dma.read(self.layout.ctrl_data, &mut answer).is_err() {
            return true;
        }
        answer[0] == HID_PROTOCOL_BOOT
    }

    /// Read the HID `interface`'s Report Descriptor over EP0, or an empty
    /// descriptor when the interface declares none or the standard
    /// `GET_DESCRIPTOR(Report)` read STALLs — the caller then has nothing to
    /// parse and uses boot protocol. A refused descriptor never fails the
    /// enumeration; only a non-STALL controller fault does.
    ///
    /// # Errors
    ///
    /// A non-STALL controller/device fault, or a register-window fault
    /// reading the delivered bytes.
    fn read_report_descriptor(
        &mut self,
        interface: &InterfaceInfo,
    ) -> Result<ReportDescriptor, DriverError> {
        let declared = usize::from(interface.report_descriptor_len);
        if declared == 0 {
            return Ok(ReportDescriptor::none());
        }
        let want = declared.min(REPORT_DESCRIPTOR_LEN);
        let want_u16 = u16::try_from(want).map_err(|_| DriverError::LengthOutOfRange)?;
        let want_u32 = u32::try_from(want).map_err(|_| DriverError::LengthOutOfRange)?;
        self.stage = EnumStage::GetReportDescriptor;
        let setup = setup_get_report_descriptor(interface.interface_number, want_u16);
        // A device that refuses the request (a STALL) keeps boot protocol.
        let transferred = match self.control(setup, want_u32) {
            Ok(transferred) => usize::try_from(transferred).unwrap_or(0),
            Err(DriverError::EndpointStalled) => return Ok(ReportDescriptor::none()),
            Err(other) => return Err(other),
        };
        let read = transferred.min(want);
        if read == 0 {
            return Ok(ReportDescriptor::none());
        }
        let mut buf = [0u8; REPORT_DESCRIPTOR_LEN];
        self.dma.read(self.layout.ctrl_data, &mut buf[..read])?;
        Ok(ReportDescriptor::new(&buf[..read]))
    }

    /// Plan which interfaces of the decoded set are served and at which
    /// device-table index: the first servable one takes the caller's
    /// `index` (whose entry and region the caller already claimed); each
    /// further one — a composite device's sibling function, e.g. the mouse
    /// interface of a wireless keyboard+mouse receiver — claims its own
    /// table entry and region ([`Self::claim_device_entry`]), sharing the
    /// device's slot and EP0. An interface the claim cannot supply memory
    /// for is left unserved rather than displacing a live device (the
    /// failed-enumeration sweep releases anything a later fault strands).
    /// A hub, or an interface this engine serves no transfer type for, is
    /// not planned.
    fn plan_interfaces(
        &mut self,
        index: usize,
        interfaces: &[Option<InterfaceInfo>; MAX_INTERFACES],
    ) -> [Option<(usize, InterfaceInfo)>; MAX_INTERFACES] {
        let mut plan: [Option<(usize, InterfaceInfo)>; MAX_INTERFACES] = [None; MAX_INTERFACES];
        let mut planned = 0usize;
        for iface in interfaces.iter().flatten() {
            if !iface.is_servable() {
                continue;
            }
            let target = if planned == 0 {
                index
            } else {
                let Ok(claimed) = self.claim_device_entry() else {
                    break;
                };
                claimed
            };
            plan[planned] = Some((target, *iface));
            planned += 1;
        }
        plan
    }

    /// Configure one planned interface's endpoints in its `target` index's
    /// own ring region, returning the built rings. A HID interface gets
    /// its interrupt-IN endpoint; a bulk interface gets its bulk endpoints
    /// plus — when it declares one — its interrupt-IN endpoint too (a CBI
    /// mass-storage interface's command-completion channel). A hub
    /// uses only its control endpoint (arming a hub's status-change
    /// endpoint wedges its EP0 ring — see [`Self::finish_enumeration`]). Every
    /// slot-context write carries `max_dci` as Context Entries so a
    /// sibling's already-configured endpoint is never shrunk out of scope.
    fn configure_interface(
        &mut self,
        slot: u8,
        base: SlotCtxBase,
        target: usize,
        iface: &InterfaceInfo,
        max_dci: u8,
    ) -> Result<ConfiguredRings, DriverError> {
        let region = self.device_region(target)?;
        if !iface.is_hid() {
            // A non-HID interface carrying the bulk endpoint pair (e.g. a
            // mass-storage interface) gets its bulk endpoints configured;
            // its transfers are then served over the bulk URB path. An
            // interrupt-IN endpoint beside them (the CBI completion
            // channel) is configured with its own ring, polled over the
            // interrupt URB path exactly like a HID report endpoint.
            let pair = self.configure_bulk_endpoints(slot, base, iface, region, max_dci)?;
            let int_ring = if iface.int_dci == DCI_CONTROL {
                None
            } else {
                Some(self.configure_interrupt_endpoint(slot, base, iface, region, max_dci)?)
            };
            return Ok(ConfiguredRings {
                int_ring,
                bulk_rings: Some(pair),
            });
        }
        let ring = self.configure_interrupt_endpoint(slot, base, iface, region, max_dci)?;
        Ok(ConfiguredRings {
            int_ring: Some(ring),
            bulk_rings: None,
        })
    }

    /// Build the interface's interrupt-IN ring in `region` and configure
    /// the endpoint the descriptor reports (DCI, max packet size, service
    /// interval — never assumed), returning the built ring only after the
    /// controller accepts the command.
    fn configure_interrupt_endpoint(
        &mut self,
        slot: u8,
        base: SlotCtxBase,
        iface: &InterfaceInfo,
        region: DeviceRegion,
        max_dci: u8,
    ) -> Result<ProducerRing, DriverError> {
        // Zero the ring first: a region reused after a detach must start
        // from a clean producer state (stale TRBs at the producer cycle
        // would be consumed past the new enqueue pointer).
        let zeros = [0u8; trb::TRB_LEN];
        for ring_slot in 0..RING_TRBS {
            self.dma
                .write(region.int_ring + ring_slot * trb::TRB_LEN, &zeros)?;
        }
        let ring_base = self.phys_of(region.int_ring)?;
        let (ring, link) = ProducerRing::new(RING_TRBS, ring_base)?;
        self.dma.write(
            region.int_ring + ring.link_slot() * trb::TRB_LEN,
            &link.to_bytes(),
        )?;
        let max_packet = u32::from(iface.int_max_packet);
        let mut interval = interrupt_interval(base.speed, iface.int_b_interval);
        // Cap a mouse's poll rate: a gaming mouse advertising a 1 ms (1000 Hz)
        // interval otherwise wakes the HCD and its class driver a thousand
        // times a second while moving. Only ever slows an aggressive endpoint
        // down to the pointer cap; a mouse polling slower keeps its interval,
        // and a keyboard is never touched.
        if iface.is_mouse() {
            interval = interval.max(pointer_min_interval());
        }
        self.write_input_ctx(
            0,
            &input_control_dwords(1 | (1u32 << u32::from(iface.int_dci))),
        )?;
        self.write_input_ctx(1, &slot_ctx_dwords(base, u32::from(max_dci)))?;
        self.write_input_ctx(
            1 + usize::from(iface.int_dci),
            &ep_ctx_dwords(EP_TYPE_INTERRUPT_IN, max_packet, interval, ring_base),
        )?;
        self.stage = EnumStage::ConfigureEndpoint;
        self.command(Trb::new(
            TrbType::ConfigureEndpoint,
            self.phys_of(self.layout.input_ctx)?,
            0,
            trb::control_slot(slot),
        ))?;
        Ok(ring)
    }

    /// Install one freshly enumerated, served interface into the table at
    /// `index`. The device's EP0 ring is the active control-context cursor
    /// right now, so the entry is recorded with `ep0_ring: None` (active,
    /// not parked); the caller parks it into the *primary* entry
    /// (`rest_active_context` via `active_device`) once the hub must be
    /// reactivated, and a root-attached device simply stays active. A
    /// composite sibling entry shares the primary's slot, output context,
    /// and EP0 offsets and never itself holds the parked ring — its control
    /// transfers route through the slot's EP0 owner
    /// ([`Self::ep0_owner_index`]).
    #[allow(clippy::too_many_arguments)] // The one construction site's facts.
    #[allow(clippy::similar_names)] // The `*2` names are the second pipes'
                                    // own names beside their primaries — deliberate siblings.
    fn install_device_entry(
        &mut self,
        index: usize,
        slot: u8,
        hub_port: u8,
        root_port: u8,
        parent_hub: usize,
        region: DeviceRegion,
        descriptor: DeviceDescriptor,
        interface: &InterfaceInfo,
        int_dci: u8,
        int_ring: Option<ProducerRing>,
        hid: HidSetup,
        bulk_rings: Option<BulkRings>,
    ) {
        let (bulk_in_ring, bulk_out_ring, bulk_in2_ring, bulk_out2_ring) = match bulk_rings {
            Some(rings) => (
                Some(rings.in_ring),
                Some(rings.out_ring),
                rings.in2_ring,
                rings.out2_ring,
            ),
            None => (None, None, None, None),
        };
        // A DCI is recorded exactly when its ring went live, so the
        // transfer paths and event attribution agree on which endpoints
        // exist.
        let bulk_in_dci = bulk_in_ring.as_ref().map_or(0, |_| interface.bulk_in_dci);
        let bulk_out_dci = bulk_out_ring.as_ref().map_or(0, |_| interface.bulk_out_dci);
        let bulk_in2_dci = bulk_in2_ring.as_ref().map_or(0, |_| interface.bulk_in2_dci);
        let bulk_out2_dci = bulk_out2_ring
            .as_ref()
            .map_or(0, |_| interface.bulk_out2_dci);
        self.devices[index] = Some(DeviceState {
            slot,
            hub_port,
            root_port,
            parent_hub,
            region,
            output_ctx: self.output_ctx_off,
            ep0_ring_off: self.ep0_ring_off,
            ep0_ring: None,
            identity: DeviceIdentity {
                vendor_id: descriptor.vendor_id,
                product_id: descriptor.product_id,
                interface_class: interface.class24,
            },
            int_dci,
            int_max_packet: interface.int_max_packet,
            int_ring,
            report_decode: hid.decode,
            reports: Fifo::new(),
            report_fault: None,
            dropped_reports: 0,
            int_recovery_pending: false,
            int_recovering: false,
            bulk_in_ring,
            bulk_out_ring,
            bulk_in2_ring,
            bulk_out2_ring,
            bulk_in_dci,
            bulk_out_dci,
            bulk_in2_dci,
            bulk_out2_dci,
            bulk_in_len: [0; BULK_SLOTS],
            bulk_out_len: [0; BULK_SLOTS],
            bulk_in2_len: [0; BULK_SLOTS],
            bulk_out2_len: [0; BULK_SLOTS],
            pending_bulk: Fifo::new(),
            aborted_bulk: Fifo::new(),
            last_report_fault_code: 0,
            report_descriptor_len: interface.report_descriptor_len,
            report_descriptor: hid.descriptor,
        });
    }

    /// Build one bulk transfer ring at region offset `ring_off`, zeroing
    /// it first: a re-enumeration reuses the memory, and stale TRBs at the
    /// producer cycle would be consumed past the new enqueue pointer.
    fn build_bulk_ring(&mut self, ring_off: usize) -> Result<ProducerRing, DriverError> {
        let zeros = [0u8; trb::TRB_LEN];
        for slot_index in 0..BULK_RING_TRBS {
            self.dma
                .write(ring_off + slot_index * trb::TRB_LEN, &zeros)?;
        }
        let base = self.phys_of(ring_off)?;
        let (ring, link) = ProducerRing::new(BULK_RING_TRBS, base)?;
        self.dma
            .write(ring_off + ring.link_slot() * trb::TRB_LEN, &link.to_bytes())?;
        Ok(ring)
    }

    /// Configure the enumerated interface's bulk endpoints (§4.6.6) inside
    /// `region`: the bulk-IN/OUT pair every bulk interface carries, plus —
    /// when the interface declares them — the second pair a UAS
    /// interface's four pipes need. Builds each transfer ring, writes the
    /// input context adding every DCI with Context Entries raised to
    /// `max_dci` (the highest DCI any of the device's served interfaces
    /// uses, so a sibling's endpoint is never shrunk out of scope), and
    /// issues one Configure Endpoint. The rings are returned — and go
    /// live in the caller's device-table entry — only after the controller
    /// accepts the command, so a refused configure leaves no half-armed
    /// bulk state.
    #[allow(clippy::similar_names)] // The `*2` names are the second pipes'
                                    // own names beside their primaries — deliberate siblings.
    fn configure_bulk_endpoints(
        &mut self,
        slot: u8,
        base: SlotCtxBase,
        interface: &InterfaceInfo,
        region: DeviceRegion,
        max_dci: u8,
    ) -> Result<BulkRings, DriverError> {
        let in_ring = self.build_bulk_ring(region.bulk_in_ring)?;
        let out_ring = self.build_bulk_ring(region.bulk_out_ring)?;
        // The second pair is configured only whole: a UAS interface
        // declares two endpoints per direction, and a lone extra endpoint
        // is left unserved rather than half-configured.
        let secondary = interface.bulk_in2_dci != 0 && interface.bulk_out2_dci != 0;
        let (in2_ring, out2_ring) = if secondary {
            (
                Some(self.build_bulk_ring(region.bulk_in2_ring)?),
                Some(self.build_bulk_ring(region.bulk_out2_ring)?),
            )
        } else {
            (None, None)
        };

        let context_entries = u32::from(max_dci);
        let mut add_flags = 1
            | (1u32 << u32::from(interface.bulk_in_dci))
            | (1u32 << u32::from(interface.bulk_out_dci));
        if secondary {
            add_flags |= (1u32 << u32::from(interface.bulk_in2_dci))
                | (1u32 << u32::from(interface.bulk_out2_dci));
        }
        self.write_input_ctx(0, &input_control_dwords(add_flags))?;
        self.write_input_ctx(1, &slot_ctx_dwords(base, context_entries))?;
        self.write_input_ctx(
            1 + usize::from(interface.bulk_in_dci),
            &ep_ctx_dwords(
                EP_TYPE_BULK_IN,
                u32::from(interface.bulk_in_max_packet),
                0,
                self.phys_of(region.bulk_in_ring)?,
            ),
        )?;
        self.write_input_ctx(
            1 + usize::from(interface.bulk_out_dci),
            &ep_ctx_dwords(
                EP_TYPE_BULK_OUT,
                u32::from(interface.bulk_out_max_packet),
                0,
                self.phys_of(region.bulk_out_ring)?,
            ),
        )?;
        if secondary {
            self.write_input_ctx(
                1 + usize::from(interface.bulk_in2_dci),
                &ep_ctx_dwords(
                    EP_TYPE_BULK_IN,
                    u32::from(interface.bulk_in2_max_packet),
                    0,
                    self.phys_of(region.bulk_in2_ring)?,
                ),
            )?;
            self.write_input_ctx(
                1 + usize::from(interface.bulk_out2_dci),
                &ep_ctx_dwords(
                    EP_TYPE_BULK_OUT,
                    u32::from(interface.bulk_out2_max_packet),
                    0,
                    self.phys_of(region.bulk_out2_ring)?,
                ),
            )?;
        }
        self.stage = EnumStage::ConfigureEndpoint;
        self.command(Trb::new(
            TrbType::ConfigureEndpoint,
            self.phys_of(self.layout.input_ctx)?,
            0,
            trb::control_slot(slot),
        ))?;
        Ok(BulkRings {
            in_ring,
            out_ring,
            in2_ring,
            out2_ring,
        })
    }

    /// Bring the controller up to serve **every** device reachable through
    /// it: every connected root-hub port, transparently descending through
    /// USB hubs — including a hub plugged into a hub, up to
    /// [`MAX_HUB_DEPTH`] tiers — **whether or not a device is connected
    /// yet**.
    ///
    /// This is the arch-neutral bring-up orchestration the host-controller
    /// driver runs once after [`Self::start`]. xHCI numbers root-hub ports
    /// from `1`. Port Power is asserted on every port first (the
    /// [`Xhci::open`] reset cleared `PORTSC`, and a port-power-controlled
    /// controller reports a powered-off port as disconnected, xHCI 1.2
    /// §4.19.1.1), the powered ports are given the power-on-good +
    /// attach-debounce window to report a connect — parked between scans,
    /// never spun — and then **every** connected root port is attached
    /// (`Self::attach_root_port`):
    ///
    /// * A root device that is itself a hub (the Raspberry Pi 4's onboard
    ///   USB2 hub tier is one — its downstream ports carry the low/full/
    ///   high-speed side of every USB-A jack) is installed and descended
    ///   (`Self::descend_hub`): its ports are powered and every connected
    ///   one attached — a further hub is installed, watched, and descended
    ///   in turn, a leaf device served, each on its own demand-allocated
    ///   region, bounded only by the controller's reported slots. Every
    ///   hub's status-change watch is armed, so later connects/disconnects
    ///   on any tier arrive through [`Self::next_hub_change`].
    /// * A root device that is a leaf (a `SuperSpeed` device trains straight
    ///   on a root port — on the Pi 4 the USB3 side of every jack is such a
    ///   port) is served directly, concurrently with every hub tier.
    /// * An empty port stays unserved; its later connect arrives through
    ///   the root-port scan ([`Self::next_root_change`]).
    ///
    /// The walk is per-port fail-soft, exactly like a hub descent: one
    /// port's broken device is skipped (counted in
    /// [`Self::skipped_port_count`], its first failure snapshotted in
    /// [`Self::last_attach_fault`]) and the remaining ports are still
    /// served. Nothing connected at boot is a first-class state, never a
    /// bring-up failure: the controller comes up serving nothing and the
    /// first hot-plug connect arrives event-driven (never polled, never
    /// spinning). The served devices afterwards are the live
    /// [`Self::device_live`] indices; the engine holds no logging
    /// dependency, so a driver wraps this with its own diagnostics.
    ///
    /// `delay` supplies the hardware-dictated settle windows (hub
    /// power-on-good and reset-recovery); the caller owns the clock.
    ///
    /// # Errors
    ///
    /// * [`Xhci::set_port_power`] or a faulting port-status read during the
    ///   connect window (the controller itself is broken).
    /// * The first attach failure — but only when **no** connected port
    ///   attached at all, so a boot diagnostic names what failed; with any
    ///   port served, individual failures are skips, not errors.
    pub fn bring_up(&mut self, delay: &dyn Delay) -> Result<(), DriverError> {
        self.skipped_ports = 0;
        self.attach_fault = None;
        let max_ports = self.xhci.max_ports();
        for port in 1..=max_ports {
            self.xhci.set_port_power(port)?;
        }
        // Allow the powered ports the power-on-good + attach-debounce
        // window to report a connect, parking between scans (a connect
        // posts a Port Status Change Event, so the controller interrupt
        // wakes the scan early). An empty controller spends the window
        // parked — never spinning — and comes up serving nothing.
        let deadline = self.wait.now_us().saturating_add(CONNECT_WINDOW_US);
        loop {
            let mut any_connected = false;
            for port in 1..=max_ports {
                if self.xhci.port_status(port)?.connected() {
                    any_connected = true;
                    break;
                }
            }
            let now = self.wait.now_us();
            if any_connected || now >= deadline {
                break;
            }
            self.wait.wait_us(deadline - now);
        }
        // Attach every connected root port, consuming each port's connect
        // latch either way so the steady-state root scan
        // ([`Self::next_root_change`]) reacts only to *new* changes.
        let mut attached = 0u32;
        let mut first_failure = None;
        for port in 1..=max_ports {
            let _ = self.xhci.clear_port_connect_change(port);
            let Ok(status) = self.xhci.port_status(port) else {
                continue;
            };
            if !status.connected() {
                continue;
            }
            match self.attach_root_port(port, delay) {
                Ok(_) => attached += 1,
                Err(err) => {
                    self.skipped_ports = self.skipped_ports.saturating_add(1);
                    if first_failure.is_none() {
                        first_failure = Some(err);
                    }
                }
            }
        }
        match (attached, first_failure) {
            // Every connected port failed to attach: surface the first
            // failure so the boot diagnostic names it.
            (0, Some(err)) => Err(err),
            _ => Ok(()),
        }
    }

    /// Attach whatever is connected on root-hub `port`: reset the port when
    /// the protocol requires it (a USB2 port enables only through a reset;
    /// a `SuperSpeed` port trains and enables on its own and is left alone),
    /// enumerate the device on its own claimed table entry and region, and
    /// serve it — a hub is installed, descended, and watched
    /// ([`Self::descend_hub`]), a leaf device served directly. The shared
    /// attach core of the bring-up walk ([`Self::bring_up`]) and the
    /// root-port hot-plug scan ([`Self::next_root_change`]).
    ///
    /// On any failure the diagnostics are snapshotted
    /// ([`Self::last_attach_fault`], with the *root* port number) before
    /// the error is surfaced, mirroring [`Self::attach_hub_port`]; the
    /// caller owns the port's connect latch. A re-attach is a brand-new
    /// enumeration: a fresh slot, no reuse of any prior device state.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Busy`] if the port already carries a served
    ///   attachment (never double-attach on a connect glitch).
    /// * [`DriverError::DeviceFault`] if the port reports no device, never
    ///   comes back enabled from its reset, or any command/transfer faults.
    /// * [`DriverError::NoSpace`] if the device table is full.
    /// * [`DriverError::BadMagic`] if a descriptor is forged.
    pub(crate) fn attach_root_port(
        &mut self,
        port: u8,
        delay: &dyn Delay,
    ) -> Result<AttachOutcome, DriverError> {
        let result = self.reset_confirm_and_attach_root(port, delay);
        if let Err(err) = result {
            // Snapshot the failure diagnostics before anything else runs:
            // the first failure is the one the per-port fail-soft walk
            // surfaces. `port_status` stays 0 — it carries a hub-format
            // `wPortStatus`, which a root port does not have; the root
            // port's raw `PORTSC` is available to the driver's diagnostics
            // through [`Self::root_port_status_raw`].
            if self.attach_fault.is_none() {
                self.attach_fault = Some(AttachFault {
                    port,
                    error: err,
                    stage: self.stage,
                    completion: self.last_completion,
                    event_type: self.last_event_type,
                    reject: self.last_reject,
                    port_status: 0,
                });
            }
        }
        result
    }

    /// The attach core of [`Self::attach_root_port`]: everything up to —
    /// but not including — descending a freshly installed root hub.
    fn reset_confirm_and_attach_root(
        &mut self,
        port: u8,
        delay: &dyn Delay,
    ) -> Result<AttachOutcome, DriverError> {
        let outcome = self.attach_root_on_port(port)?;
        // A freshly installed root hub is descended only now, with the
        // cursor rested, so its own attach failures can never wedge
        // another tier's watch. A tier that cannot be powered or watched
        // is torn down whole rather than left half-installed.
        if let AttachOutcome::Hub(new_hub) = outcome {
            if let Err(err) = self.descend_hub(new_hub, delay) {
                let _ = self.detach_hub(new_hub);
                return Err(err);
            }
        }
        Ok(outcome)
    }

    /// Confirm the connect on root-hub `port`, reset the port when it is
    /// not already enabled (a USB2 port enables only through a reset; a
    /// `SuperSpeed` port trains on its own), and enumerate and serve the
    /// device on a fresh table entry and region — a hub is installed and
    /// watched-ready but **not** yet descended (the caller descends it, so
    /// its downstream failures never wedge this attach). The slot-level
    /// stage every root attach shares.
    ///
    /// # Errors
    ///
    /// As [`Self::attach_root_port`], minus the descend.
    pub(crate) fn attach_root_on_port(&mut self, port: u8) -> Result<AttachOutcome, DriverError> {
        if self.root_attachment_on(port).is_some() {
            // The port already carries a served attachment (a connect
            // glitch, or a repeated scan): never double-attach.
            return Err(DriverError::Busy);
        }
        self.stage = EnumStage::PortReset;
        self.last_attach_status = 0;
        let status = self.xhci.port_status(port)?;
        if !status.connected() {
            return Err(DriverError::DeviceFault);
        }
        let status = if status.enabled() {
            status
        } else {
            self.xhci.reset_port(port, self.budget)?
        };
        let speed = status.speed();
        let max_packet = ep0_max_packet(speed)?;
        let index = self.claim_device_entry()?;
        self.rebind_to_device_region(index)?;
        let result = self.attach_on_rebound_region(index, None, port, speed, max_packet);
        // Rest the control cursor off the just-touched entry whether or
        // not the attach succeeded — no hub watch may lose its ring — and
        // release every claim nothing owns, so no attach outcome leaks DMA.
        let rested = self.rest_active_context();
        self.release_unattached_regions();
        let outcome = result?;
        rested?;
        Ok(outcome)
    }

    /// The attachment served on root-hub `port`: the root-attached hub
    /// whose tier sits there, or the directly-attached device, or `None`
    /// while the port is unserved. Composite sibling entries share the
    /// port; the first is returned (detaching it detaches its siblings).
    fn root_attachment_on(&self, port: u8) -> Option<RootAttachment> {
        if let Some(hub_index) = self.hubs.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(|hub| hub.parent.is_none() && hub.root_port == port)
        }) {
            return Some(RootAttachment::Hub(hub_index));
        }
        if let Some(index) = self.devices.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(|device| device.hub_port == 0 && device.root_port == port)
        }) {
            return Some(RootAttachment::Device(index));
        }
        None
    }

    /// Service one root-hub port connect/disconnect change, returning what
    /// changed — the root-port counterpart of [`Self::next_hub_change`].
    ///
    /// Called by the HCD whenever the controller interrupt fires: a
    /// connect or disconnect on a root port latches `PORTSC.CSC` (and
    /// posts the Port Status Change Event that raised the interrupt), so
    /// the scan reads each port's latch, consumes it, and reconciles the
    /// port against what is currently served — a new connect on an
    /// unserved port is attached (`Self::attach_root_port`: a hub tier
    /// installed, descended, and watched, or a leaf device served), and a
    /// disconnect detaches exactly what that port carried (a hub tier with
    /// everything behind it, or the directly-attached device). Entirely
    /// event-driven — with no latch set it returns [`HubEvent::None`],
    /// and it never polls or spins.
    ///
    /// The `PORTSC` walk itself runs only when a root-port change has armed
    /// it, which is what keeps a steady stream of report interrupts
    /// (a mouse in motion) off the port registers entirely: on a PCIe
    /// controller each `PORTSC` read is a non-posted round trip and dwarfs
    /// the rest of the report path. Two independent latched sources arm it —
    /// a Port Status Change Event drained by *any* ring consumer, and the
    /// `USBSTS.PCD` summary — and neither can lose an edge, so a plug is
    /// still seen even when its event was consumed by an engine wait.
    ///
    /// The scan is per-port fail-soft, mirroring [`Self::next_hub_change`]:
    /// one port's broken device has its latch consumed and the remaining
    /// changed ports are still serviced; the first failure is surfaced
    /// (the caller logs it, with [`Self::last_attach_fault`] naming the
    /// port) only when no actionable event was found.
    ///
    /// `delay` supplies the enumeration settle windows on a fresh connect;
    /// the caller owns the clock.
    ///
    /// # Errors
    ///
    /// [`DriverError`] from the first failed attach or detach when no
    /// actionable event was produced (fail closed).
    pub fn next_root_change(&mut self, delay: &dyn Delay) -> Result<HubEvent, DriverError> {
        if !self.root_change_pending {
            return Ok(HubEvent::None);
        }
        self.attach_fault = None;
        let max_ports = self.xhci.max_ports();
        let mut first_failure = None;
        for port in 1..=max_ports {
            let Ok(status) = self.xhci.port_status(port) else {
                continue;
            };
            if !status.connect_changed() {
                continue;
            }
            // Consume the latch before acting, so a failed attach cannot
            // re-trigger forever on a stale latch (a genuine re-plug
            // latches it anew) — the root-port analogue of draining a
            // hub port's changes.
            if self.xhci.clear_port_connect_change(port).is_err() {
                continue;
            }
            // Reconcile against the *current* connect state, re-read after
            // the latch was consumed: the latch says something changed,
            // the live state says what the port carries now.
            let connected = self.xhci.port_status(port).is_ok_and(PortStatus::connected);
            match (self.root_attachment_on(port), connected) {
                (Some(RootAttachment::Hub(hub_index)), false) => {
                    self.detach_hub(hub_index)?;
                    return Ok(HubEvent::HubDetached(hub_index));
                }
                (Some(RootAttachment::Device(index)), false) => {
                    self.detach_device(index)?;
                    return Ok(HubEvent::Detached(index));
                }
                (None, true) => match self.attach_root_port(port, delay) {
                    Ok(AttachOutcome::Hub(hub_index)) => {
                        return Ok(HubEvent::HubAttached(hub_index))
                    }
                    Ok(AttachOutcome::Device(index)) => return Ok(HubEvent::Attached(index)),
                    Err(err) => {
                        if first_failure.is_none() {
                            first_failure = Some(err);
                        }
                    }
                },
                // A connect glitch on a served port (its transfers fault
                // and the fault path detaches it if the device really
                // changed), or a flicker on an unserved one: drained.
                (Some(_), true) | (None, false) => {}
            }
        }
        if let Some(err) = first_failure {
            return Err(err);
        }
        // Only a scan that walked every port and found nothing actionable
        // disarms: an actionable event returns from inside the loop above with
        // later ports unvisited, and the caller re-scans until it sees `None`.
        self.root_change_pending = false;
        Ok(HubEvent::None)
    }

    /// Reset the hub at `hub_index`'s downstream `port`, await the reset
    /// completing ([`Self::await_port_reset_complete`]), and attach
    /// whatever is behind it ([`Self::attach_downstream_device`]) — a leaf
    /// device, or a further hub tier that is installed and descended.
    ///
    /// On **any** failure the diagnostics are snapshotted
    /// ([`Self::last_attach_fault`]) and then the port's latched changes
    /// are drained (best-effort) before the error is surfaced: the reset
    /// this attach issued latches `C_PORT_RESET` (and the connect change
    /// may already be latched), and a hub keeps its status-change endpoint
    /// re-reporting the port until every latch is cleared — an undrained
    /// failed attach would make the watch re-fire, and re-run the same
    /// failing enumeration, forever (the metal fault loop that starved
    /// every other port).
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the reset port never reports
    ///   enabled within the reset-completion budget (it never established
    ///   a speed/TT, so addressing it would be a guess).
    /// * Any error of [`Self::reset_hub_port`], [`Self::hub_port_status`],
    ///   or [`Self::attach_downstream_device`].
    fn attach_hub_port(
        &mut self,
        hub_index: usize,
        port: u8,
        delay: &dyn Delay,
    ) -> Result<AttachOutcome, DriverError> {
        let result = self.reset_confirm_and_attach(hub_index, port, delay);
        if let Err(err) = result {
            // Snapshot the failure diagnostics **before** the latch drain:
            // its own transfers overwrite the live stage/completion/reject
            // state, and the first failure is the one the per-port
            // fail-soft scan surfaces.
            if self.attach_fault.is_none() {
                self.attach_fault = Some(AttachFault {
                    port,
                    error: err,
                    stage: self.stage,
                    completion: self.last_completion,
                    event_type: self.last_event_type,
                    reject: self.last_reject,
                    port_status: self.last_attach_status,
                });
            }
            // Best-effort: the device just failed, so these hub-class
            // transfers may fail too; the attach error is the one surfaced.
            if let Ok((_, change)) = self.hub_port_status_change(hub_index, port) {
                let _ = self.clear_hub_port_changes(hub_index, port, change);
            }
        }
        result
    }

    /// The attach core of [`Self::attach_hub_port`]: reset the port so the
    /// hub enables it and establishes its speed and transaction translator,
    /// await the reset completing ([`Self::await_port_reset_complete`]),
    /// and attach the device behind it.
    fn reset_confirm_and_attach(
        &mut self,
        hub_index: usize,
        port: u8,
        delay: &dyn Delay,
    ) -> Result<AttachOutcome, DriverError> {
        self.stage = EnumStage::PortReset;
        self.last_attach_status = 0;
        self.reset_hub_port(hub_index, port)?;
        let status = self.await_port_reset_complete(hub_index, port, delay)?;
        // A `SuperSpeed` hub's ports carry only `SuperSpeed` devices; its
        // `wPortStatus` reserves the USB 2.0 speed bits as zero (USB 3.2
        // §10.16.2.6), so decoding them would misread the device as
        // full-speed and address it with the wrong EP0 packet size.
        let hub_speed = self.hub(hub_index).ok_or(DriverError::DeviceFault)?.speed;
        let speed = if hub_speed == SPEED_SUPER {
            SPEED_SUPER
        } else {
            hub_port_speed(status)
        };
        self.attach_downstream_device(hub_index, port, speed, delay)
    }

    /// Await the hub completing a downstream `port` reset: poll the port's
    /// `wPortStatus` (USB 2.0 §11.24.2.7) at [`HUB_RESET_POLL_US`] spacing —
    /// each interval parked on `delay`, never spun — until the hub reports
    /// the reset signalling done and the port enabled, then wait the
    /// `TRSTRCY` recovery settle and return the final status (its speed
    /// bits select the downstream device's protocol speed).
    ///
    /// A hub exposes no interrupt for its own reset completion while its
    /// status-change report is being serviced, so this bounded re-poll is
    /// the protocol's completion signal; a single fixed wait is wrong both
    /// ways (too short for a slow hub, a needless stall for a fast one).
    /// Every observed status is recorded so a failed attach's
    /// [`AttachFault::port_status`] shows the port's final state.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the port does not report enabled
    ///   within [`HUB_RESET_POLLS`] polls (the device never established a
    ///   speed/TT, so addressing it would be a guess — fail closed).
    /// * Any error of [`Self::hub_port_status`].
    fn await_port_reset_complete(
        &mut self,
        hub_index: usize,
        port: u8,
        delay: &dyn Delay,
    ) -> Result<u16, DriverError> {
        for _ in 0..HUB_RESET_POLLS {
            delay.delay_us(HUB_RESET_POLL_US);
            let status = self.hub_port_status(hub_index, port)?;
            self.last_attach_status = status;
            if !hub_port_resetting(status) && hub_port_enabled(status) {
                delay.delay_us(HUB_RESET_SETTLE_US);
                return Ok(status);
            }
        }
        Err(DriverError::DeviceFault)
    }

    /// Number of root-hub ports the controller reports
    /// (`HCSPARAMS1` `MaxPorts`).
    ///
    /// For a one-shot diagnostic that walks every root-hub port's
    /// `PORTSC` ([`Self::root_port_status_raw`]).
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
    /// The request mirrors what production stacks (Linux `hub.c`, Windows)
    /// issue: the full base-descriptor size, retried a bounded number of
    /// times when the hub answers wrongly. A truncated 8-byte read is an
    /// exchange no mainstream host ever sends, and real hubs (a Realtek
    /// RTS5411 on the Pi 4) answered it with garbage — the reply passed
    /// the transfer but failed the type check, and the whole tier behind
    /// the hub went unserved.
    ///
    /// `superspeed` selects the descriptor a `SuperSpeed` hub actually
    /// serves — the fixed 12-byte [`DESC_TYPE_SS_HUB`] one (USB 3.2
    /// §10.15.2.1); an SS hub STALLs a request for the USB 2.0
    /// [`DESC_TYPE_HUB`] descriptor, which is how a whole USB3-attached
    /// tier went unserved on the Pi 4's `SuperSpeed` root port. Both
    /// layouts carry `bNbrPorts` at byte 2 and `wHubCharacteristics` at
    /// bytes 3:4; a `SuperSpeed` hub has no transaction translator, so its
    /// TT Think Time is reported as zero.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] for a non-hub or too-short reply on
    ///   every attempt.
    /// * [`DriverError::EndpointStalled`] if the hub refused the request
    ///   on every attempt (the control endpoint is already recovered).
    /// * [`DriverError::DeviceFault`] if the control transfer faults.
    fn read_hub_topology(&mut self, superspeed: bool) -> Result<(u8, u8), DriverError> {
        // The USB 2.0 hub descriptor's fixed head plus the two variable
        // port-bitmap fields at their smallest (§11.23.2.1) — the size
        // Linux requests; a hub with more ports answers with a short
        // packet's honest byte count, and the fields this driver needs
        // (`bNbrPorts`, `wHubCharacteristics`) sit in the first five bytes.
        const HUB_DESC_REQUEST: usize = 15;
        let (desc_type, request) = if superspeed {
            (DESC_TYPE_SS_HUB, SS_HUB_DESC_LEN)
        } else {
            (DESC_TYPE_HUB, HUB_DESC_REQUEST)
        };
        let want = u16::try_from(request).map_err(|_| DriverError::LengthOutOfRange)?;
        let mut last = DriverError::BadMagic;
        for _ in 0..HUB_DESC_ATTEMPTS {
            // Zero the staging bytes first so a reply that moves fewer
            // bytes than claimed can never be validated against a stale
            // earlier transfer's leftovers.
            self.dma
                .write(self.layout.ctrl_data, &[0u8; HUB_DESC_REQUEST])?;
            let transferred =
                match self.control(setup_get_hub_descriptor(desc_type, want), u32::from(want)) {
                    Ok(transferred) => transferred,
                    // The hub answered the request wrongly (a refusal STALL —
                    // EP0 is already recovered). Transport/controller faults
                    // are not retried: a timeout compounds and a fault will
                    // not heal.
                    Err(err @ DriverError::EndpointStalled) => {
                        last = err;
                        continue;
                    }
                    Err(err) => return Err(err),
                };
            if (transferred as usize) < 5 {
                last = DriverError::BadMagic;
                continue;
            }
            let mut desc = [0u8; HUB_DESC_REQUEST];
            self.dma.read(self.layout.ctrl_data, &mut desc)?;
            if desc[1] != desc_type {
                last = DriverError::BadMagic;
                continue;
            }
            // A `SuperSpeed` hub has no TT; its characteristics bits 5:6 are
            // reserved, never a think time.
            let tt_think_time = if superspeed {
                0
            } else {
                ((u16::from_le_bytes([desc[3], desc[4]]) >> 5) & 0b11) as u8
            };
            return Ok((desc[2], tt_think_time));
        }
        Err(last)
    }

    /// Read a configured hub's `bNbrPorts` (downstream port count) from its
    /// hub class descriptor (USB 2.0 §11.23.2.1 / USB 3.2 §10.15.2.1,
    /// selected by the active hub's own protocol speed).
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] for a non-hub or too-short reply.
    /// * [`DriverError::DeviceFault`] if the control transfer faults.
    pub fn hub_num_ports(&mut self) -> Result<u8, DriverError> {
        let superspeed = self
            .active_hub
            .and_then(|index| self.hub(index))
            .is_some_and(|hub| hub.speed == SPEED_SUPER);
        Ok(self.read_hub_topology(superspeed)?.0)
    }

    /// Set the **Hub** bit in the active slot's context (xHCI §6.2.2) so the
    /// controller routes and splits the transactions of devices addressed
    /// downstream of it — otherwise a device behind the hub is addressed
    /// but never delivers a report. Issues an `A0`-only Configure Endpoint
    /// copying the live output slot context and setting the Hub bit, Number
    /// of Ports, and TT Think Time from the hub descriptor (single-TT).
    /// Must run while the hub is the active slot, before
    /// [`Self::rebind_to_device_region`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] if the hub descriptor is forged.
    /// * [`DriverError::DeviceFault`] if the controller rejects the command.
    ///
    /// Returns the hub's `(bNbrPorts, TT Think Time)` so the caller records
    /// the topology it just programmed without a second descriptor read.
    /// `superspeed` selects the hub descriptor the hub actually serves
    /// ([`Self::read_hub_topology`]); a `SuperSpeed` hub has no TT, so its
    /// slot's TT Think Time is programmed zero.
    fn configure_hub_slot(&mut self, superspeed: bool) -> Result<(u8, u8), DriverError> {
        let (num_ports, tt_think_time) = self.read_hub_topology(superspeed)?;
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
            self.phys_of(self.layout.input_ctx)?,
            0,
            trb::control_slot(self.slot),
        ))?;
        Ok((num_ports, tt_think_time))
    }

    /// Assert `PORT_POWER` on the hub at `hub_index`'s downstream `port`
    /// (1-based) via a class `SET_FEATURE` (USB 2.0 §11.24.2.13). A
    /// port-power-controlled hub reports a port disconnected until this is
    /// set; the caller waits the power-on-good time before reading
    /// [`Self::hub_port_status`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if no hub is live at `hub_index`.
    /// * [`DriverError::DeviceFault`] if the control transfer faults.
    pub fn power_hub_port(&mut self, hub_index: usize, port: u8) -> Result<(), DriverError> {
        self.hub_control(
            hub_index,
            setup_set_port_feature(PORT_FEATURE_POWER, port),
            0,
        )
        .map(|_| ())
    }

    /// Read the hub at `hub_index`'s downstream `port` 16-bit `wPortStatus`
    /// via a class `GET_STATUS` (USB 2.0 §11.24.2.7).
    ///
    /// Decode it with [`hub_port_connected`] and [`hub_port_speed`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if no hub is live at `hub_index`.
    /// * [`DriverError::DeviceFault`] if the control transfer faults or
    ///   the device returns fewer than the two `wPortStatus` bytes
    ///   (fail closed).
    pub fn hub_port_status(&mut self, hub_index: usize, port: u8) -> Result<u16, DriverError> {
        let transferred = self.hub_control(hub_index, setup_get_port_status(port), 4)?;
        if transferred < 2 {
            return Err(DriverError::DeviceFault);
        }
        let mut buf = [0u8; 4];
        self.dma.read(self.layout.ctrl_data, &mut buf)?;
        Ok(u16::from_le_bytes([buf[0], buf[1]]))
    }

    /// Reset the hub at `hub_index`'s downstream `port` (1-based) via a
    /// class `SET_FEATURE(PORT_RESET)` (USB 2.0 §11.24.2.13).
    ///
    /// A downstream device is enabled — and its speed (and, for a
    /// full/low-speed device, its transaction translator) established —
    /// only once its hub port has been reset. The attach path then polls
    /// the port's status until the hub reports the reset complete and the
    /// port enabled before addressing the device.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if no hub is live at `hub_index`.
    /// * [`DriverError::DeviceFault`] if the control transfer faults
    ///   (fail closed).
    pub fn reset_hub_port(&mut self, hub_index: usize, port: u8) -> Result<(), DriverError> {
        self.hub_control(
            hub_index,
            setup_set_port_feature(PORT_FEATURE_RESET, port),
            0,
        )
        .map(|_| ())
    }

    /// Clear **every** latched change on a downstream hub `port` whose
    /// `wPortChange` word is `change`, via one class `CLEAR_FEATURE` (USB 2.0
    /// §11.24.2.2) per set bit.
    ///
    /// A hub keeps its status-change endpoint asserting a report for the port
    /// until *all* its latched changes are cleared. Enumeration resets the
    /// port (`SET_FEATURE(PORT_RESET)`), which latches `C_PORT_RESET` (and the
    /// hub may latch `C_PORT_ENABLE`) alongside `C_PORT_CONNECTION`; clearing
    /// only the connect change leaves the port permanently flagged, so the
    /// freshly-armed watch fires immediately and forever on a change that is
    /// never a real hot-plug. Draining the whole set leaves the watch quiet
    /// until the next genuine connect/disconnect.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if a control transfer faults (fail closed).
    fn clear_hub_port_changes(
        &mut self,
        hub_index: usize,
        port: u8,
        change: u16,
    ) -> Result<(), DriverError> {
        // A `SuperSpeed` hub latches a different change set (warm-reset,
        // link-state, and config-error latches; no enable/suspend ones) —
        // an uncleared latch keeps the status-change watch firing forever.
        let features: &[(u16, u8)] = if self
            .hub(hub_index)
            .is_some_and(|hub| hub.speed == SPEED_SUPER)
        {
            &SS_PORT_CHANGE_FEATURES
        } else {
            &PORT_CHANGE_FEATURES
        };
        for &(bit, feature) in features {
            if change & bit != 0 {
                self.hub_control(hub_index, setup_clear_port_feature(feature, port), 0)?;
            }
        }
        Ok(())
    }

    /// Read the hub at `hub_index`'s downstream `port` `wPortStatus` and
    /// `wPortChange` words (USB 2.0 §11.24.2.7) in one class `GET_STATUS`.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if the transfer faults or returns fewer
    /// than the four status/change bytes (fail closed).
    fn hub_port_status_change(
        &mut self,
        hub_index: usize,
        port: u8,
    ) -> Result<(u16, u16), DriverError> {
        let transferred = self.hub_control(hub_index, setup_get_port_status(port), 4)?;
        if transferred < 4 {
            return Err(DriverError::DeviceFault);
        }
        let mut buf = [0u8; 4];
        self.dma.read(self.layout.ctrl_data, &mut buf)?;
        Ok((
            u16::from_le_bytes([buf[0], buf[1]]),
            u16::from_le_bytes([buf[2], buf[3]]),
        ))
    }

    /// Rebind the active default-control endpoint to device-region
    /// `index`'s ring and output context, so the next
    /// [`Self::address_device`] addresses a *downstream* device on a fresh
    /// ring and output context, leaving the hub's root-region ring and
    /// output context live in the DCBAA.
    ///
    /// Initialises the region's EP0 ring Link TRB exactly as [`Self::start`]
    /// does for the root ring, and parks the previously active EP0 ring in
    /// its owner's table entry ([`Self::park_active_ring`]) so
    /// [`Self::rest_active_context`] can make the resting hub the active
    /// control context again after the downstream device is enumerated
    /// (every hub stays addressed for status-change watching and per-port
    /// class requests).
    fn rebind_to_device_region(&mut self, index: usize) -> Result<(), DriverError> {
        let region = self.device_region(index)?;
        // Zero the region before building a fresh ring: a re-attach reuses the
        // same memory, and stale TRBs left at the producer cycle from a prior
        // device would be consumed past the new enqueue pointer (their cycle
        // bit aliases the fresh ring's), so they must be cleared first.
        let zeros = [0u8; trb::TRB_LEN];
        for slot in 0..RING_TRBS {
            self.dma
                .write(region.ep0_ring + slot * trb::TRB_LEN, &zeros)?;
        }
        let base = self.phys_of(region.ep0_ring)?;
        let (ring, link) = ProducerRing::new(RING_TRBS, base)?;
        self.dma.write(
            region.ep0_ring + ring.link_slot() * trb::TRB_LEN,
            &link.to_bytes(),
        )?;
        let previous = core::mem::replace(&mut self.ep0_ring, ring);
        self.park_active_ring(previous);
        self.ep0_ring_off = region.ep0_ring;
        self.output_ctx_off = region.output_ctx;
        Ok(())
    }

    /// Park `ring` — the EP0 cursor being switched away from — into
    /// whichever table entry owned it (the active hub or the active
    /// device), clearing the active markers. A cursor with no owner (a
    /// failed enumeration whose device was never installed) is dropped
    /// with its slot.
    fn park_active_ring(&mut self, ring: ProducerRing) {
        if let Some(hub_index) = self.active_hub.take() {
            if let Some(hub) = self.hubs.get_mut(hub_index).and_then(Option::as_mut) {
                hub.ep0_ring = Some(ring);
            }
            self.active_device = None;
            return;
        }
        if let Some(index) = self.active_device.take() {
            if let Some(device) = self.devices.get_mut(index).and_then(Option::as_mut) {
                device.ep0_ring = Some(ring);
            }
        }
    }

    /// Make the hub at `hub_index` the active control context,
    /// reactivating its parked EP0 ring and parking the previous owner's,
    /// so hub class requests (`GET_STATUS`, `SET_FEATURE`, …) target that
    /// hub. A no-op when it is already active.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if no hub is live at `hub_index`.
    /// * [`DriverError::DeviceFault`] if the hub's EP0 ring is not parked
    ///   (a caller bug — an activation was skipped or doubled).
    fn activate_hub_control(&mut self, hub_index: usize) -> Result<(), DriverError> {
        if self.active_hub == Some(hub_index) {
            return Ok(());
        }
        let hub = self
            .hubs
            .get_mut(hub_index)
            .and_then(Option::as_mut)
            .ok_or(DriverError::NotFound)?;
        let ring = hub.ep0_ring.take().ok_or(DriverError::DeviceFault)?;
        let (slot, ep0_ring_off, output_ctx_off) = (hub.slot, hub.ep0_ring_off, hub.output_ctx);
        let previous = core::mem::replace(&mut self.ep0_ring, ring);
        self.park_active_ring(previous);
        self.ep0_ring_off = ep0_ring_off;
        self.output_ctx_off = output_ctx_off;
        self.slot = slot;
        self.active_hub = Some(hub_index);
        Ok(())
    }

    /// Rest the active control context after another slot was enumerated
    /// or activated: the resting state every operation returns to, chosen
    /// so the cursor never rests on an entry a hot-removal is likely to
    /// free mid-operation.
    ///
    /// The rest target is the lowest-index live hub (a hub topology's
    /// watches must never lose their rings); with no hub installed, the
    /// lowest-index device entry holding a parked EP0 ring (the
    /// direct-attach topology); with a live device already active and
    /// nothing better, the cursor stays where it is; with nothing live at
    /// all, the cursor is rebound to the engine's own idle layout binding
    /// ([`Self::rebind_to_idle_layout`]) so it never dangles on a released
    /// region.
    ///
    /// The previously active EP0 ring is parked in its owner's table entry
    /// ([`Self::park_active_ring`]) so a later control transfer targeting
    /// it (a URB control-IN, the bulk halt recovery's `CLEAR_FEATURE`, a
    /// downstream hub's class request) can reactivate it.
    ///
    /// # Errors
    ///
    /// As [`Self::activate_hub_control`] / [`Self::activate_device_control`]
    /// for the chosen rest target (a caller bug — its ring never parked).
    fn rest_active_context(&mut self) -> Result<(), DriverError> {
        if let Some(hub_index) = self.hubs.iter().position(|entry| entry.as_ref().is_some()) {
            return self.activate_hub_control(hub_index);
        }
        if self
            .active_device
            .is_some_and(|index| self.devices.get(index).is_some_and(Option::is_some))
        {
            // A live directly-attached device is the active context and no
            // hub exists to rest on: staying put is the resting state.
            return Ok(());
        }
        if let Some(index) = self.devices.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(|device| device.ep0_ring.is_some())
        }) {
            return self.activate_device_control(index);
        }
        self.rebind_to_idle_layout()
    }

    /// Rebind the EP0 cursor to the engine's own layout ring with no slot
    /// addressed — the idle binding an emptied topology rests in (exactly
    /// the post-`start` state), so the cursor never dangles on a released
    /// device region. The previous ring is parked in its owner's entry
    /// when one still exists, else dropped with its slot.
    fn rebind_to_idle_layout(&mut self) -> Result<(), DriverError> {
        let zeros = [0u8; trb::TRB_LEN];
        for slot in 0..RING_TRBS {
            self.dma
                .write(self.layout.ep0_ring + slot * trb::TRB_LEN, &zeros)?;
        }
        let base = self.phys_of(self.layout.ep0_ring)?;
        let (ring, link) = ProducerRing::new(RING_TRBS, base)?;
        self.dma.write(
            self.layout.ep0_ring + ring.link_slot() * trb::TRB_LEN,
            &link.to_bytes(),
        )?;
        let previous = core::mem::replace(&mut self.ep0_ring, ring);
        self.park_active_ring(previous);
        self.ep0_ring_off = self.layout.ep0_ring;
        self.output_ctx_off = self.layout.output_ctx;
        self.slot = 0;
        self.active_hub = None;
        self.active_device = None;
        Ok(())
    }

    /// Index of the device-table entry holding slot `slot`'s **parked** EP0
    /// ring — the entry a control transfer for that slot is activated
    /// through. A composite device's sibling entries share the slot but
    /// never themselves hold the ring, so a sibling's control transfer
    /// routes through this owner. `None` while the slot's ring is not
    /// parked (the slot is already the active control context, or no entry
    /// holds it).
    fn ep0_owner_index(&self, slot: u8) -> Option<usize> {
        self.devices.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(|device| device.slot == slot && device.ep0_ring.is_some())
        })
    }

    /// Make the served device at `index` the active control context again,
    /// reactivating the EP0 ring [`Self::rest_active_context`] parked in its
    /// table entry, so a post-enumeration control transfer (a URB
    /// control-IN, the bulk halt recovery's `CLEAR_FEATURE`) targets the
    /// *device*, never the hub.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if no device is live at `index`.
    /// * [`DriverError::DeviceFault`] if the device's EP0 ring is not
    ///   parked (a caller bug — the device is already the active context).
    fn activate_device_control(&mut self, index: usize) -> Result<(), DriverError> {
        let device = self
            .devices
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or(DriverError::NotFound)?;
        let device_ring = device.ep0_ring.take().ok_or(DriverError::DeviceFault)?;
        let (slot, ep0_ring_off, output_ctx_off) =
            (device.slot, device.ep0_ring_off, device.output_ctx);
        let previous = core::mem::replace(&mut self.ep0_ring, device_ring);
        self.park_active_ring(previous);
        self.ep0_ring_off = ep0_ring_off;
        self.output_ctx_off = output_ctx_off;
        self.slot = slot;
        self.active_device = Some(index);
        Ok(())
    }

    /// Run a control transfer targeting the **hub** at `hub_index` rather
    /// than whatever slot is the resting active control context: the root
    /// hub is already active at rest, a downstream hub is activated for
    /// the transfer and the root hub restored after — even when the
    /// transfer itself fails, so no hub watch ever loses its ring.
    ///
    /// # Errors
    ///
    /// [`DriverError::NotFound`] with no hub live at `hub_index`, else as
    /// [`Self::control`] / [`Self::activate_hub_control`].
    fn hub_control(
        &mut self,
        hub_index: usize,
        setup: [u8; 8],
        data_in_len: u32,
    ) -> Result<u32, DriverError> {
        let hub_slot = self.hub(hub_index).ok_or(DriverError::NotFound)?.slot;
        if self.slot == hub_slot {
            return self.control(setup, data_in_len);
        }
        self.activate_hub_control(hub_index)?;
        let result = self.control(setup, data_in_len);
        let restored = self.rest_active_context();
        let transferred = result?;
        restored?;
        Ok(transferred)
    }

    /// Run a control transfer targeting the served **device** at `index`
    /// rather than whatever slot is the resting active control context: a
    /// directly-attached device is already active, a hub-downstream device
    /// is activated for the transfer and the hub restored after — even when
    /// the transfer itself fails, so the hub watch never loses its ring.
    ///
    /// # Errors
    ///
    /// [`DriverError::NotFound`] with no device live at `index`, else as
    /// [`Self::control`] / [`Self::activate_device_control`].
    fn device_control(
        &mut self,
        index: usize,
        setup: [u8; 8],
        data_in_len: u32,
    ) -> Result<u32, DriverError> {
        let device_slot = self.device(index).ok_or(DriverError::NotFound)?.slot;
        if self.slot == device_slot {
            return self.control(setup, data_in_len);
        }
        // A composite sibling entry shares its slot's EP0 with the primary
        // entry and never itself holds the parked ring; activate through
        // whichever entry owns it.
        let owner = self.ep0_owner_index(device_slot).unwrap_or(index);
        self.activate_device_control(owner)?;
        let result = self.control(setup, data_in_len);
        let restored = self.rest_active_context();
        let transferred = result?;
        restored?;
        Ok(transferred)
    }

    /// Run a control-OUT transfer (SETUP + OUT data stage + status)
    /// targeting the served **device** at `index`, with the same
    /// activate/restore discipline as [`Self::device_control`] so the hub
    /// watch never loses its ring.
    ///
    /// # Errors
    ///
    /// [`DriverError::NotFound`] with no device live at `index`, else as
    /// [`Self::control_out_transfer`] /
    /// [`Self::activate_device_control`].
    fn device_control_out(
        &mut self,
        index: usize,
        setup: [u8; 8],
        data: &[u8],
    ) -> Result<(), DriverError> {
        let device_slot = self.device(index).ok_or(DriverError::NotFound)?.slot;
        if self.slot == device_slot {
            return self.control_out_transfer(setup, data);
        }
        let owner = self.ep0_owner_index(device_slot).unwrap_or(index);
        self.activate_device_control(owner)?;
        let result = self.control_out_transfer(setup, data);
        let restored = self.rest_active_context();
        result?;
        restored
    }

    /// Install the hub addressed on the **active** control-context slot
    /// into the hub table and tell the controller the slot is a hub
    /// ([`Self::configure_hub_slot`]), so devices addressed downstream of
    /// it are routed and their split transactions scheduled (xHCI §6.2.2).
    ///
    /// The root-attached hub installs with `parent = None` during
    /// [`Self::bring_up`]; a downstream hub installs right after its
    /// enumeration, while it is still the active context, claiming the
    /// device region it was enumerated on (`device_region`). `base` is the
    /// slot-context topology the hub was addressed with — its route string,
    /// speed, and TT coordinates, which its downstream devices inherit.
    /// Consumes the status-change endpoint [`Self::finish_enumeration`]
    /// captured. The installed hub becomes the active control context.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if no device is addressed on the
    ///   active slot, or the controller rejects the Configure Endpoint.
    /// * [`DriverError::NoSpace`] if the hub table is full (the tier is
    ///   left unserved fail-closed, never displacing a watched hub).
    /// * [`DriverError::NotFound`] if `parent` names no live hub.
    /// * [`DriverError::BadMagic`] if the hub descriptor is forged.
    fn install_hub(
        &mut self,
        parent: Option<usize>,
        parent_port: u8,
        base: SlotCtxBase,
        device_region: Option<usize>,
    ) -> Result<usize, DriverError> {
        // A hub must be addressed on the active slot for the route string's
        // root-port and TT-hub-slot to be meaningful.
        if self.slot == 0 {
            return Err(DriverError::DeviceFault);
        }
        let hub_index = self.claim_hub_entry()?;
        let depth = match parent {
            None => 0,
            Some(parent_index) => self.hub(parent_index).ok_or(DriverError::NotFound)?.depth + 1,
        };
        let superspeed = base.speed == SPEED_SUPER;
        let (num_ports, _tt_think_time) = self.configure_hub_slot(superspeed)?;
        // A `SuperSpeed` hub must be told its tier depth before it can
        // decode downstream route strings (USB 3.2 §10.16.2.7); without
        // it every transaction to a device behind the hub is misrouted.
        if superspeed {
            self.control(setup_set_hub_depth(depth), 0)?;
        }
        let region = HubRegion::at(self.dma.grow(HubRegion::layout_len())?);
        self.hubs[hub_index] = Some(HubState {
            slot: self.slot,
            parent,
            parent_port,
            root_port: base.root_port,
            route_string: base.route_string,
            depth,
            speed: base.speed,
            num_ports,
            tt_hub_slot: base.tt_hub_slot,
            tt_port: base.tt_port,
            output_ctx: self.output_ctx_off,
            ep0_ring_off: self.ep0_ring_off,
            region,
            device_region,
            // The hub is the active control context, so its ring is the
            // live cursor, not parked here.
            ep0_ring: None,
            int_endpoint: self.pending_hub_endpoint.take(),
            int_dci: 0,
            int_ring: None,
            pending: None,
        });
        self.active_hub = Some(hub_index);
        self.active_device = None;
        Ok(hub_index)
    }

    /// Address and configure the device on the hub at `hub_index`'s
    /// downstream `down_port` (1-based) at protocol `speed`, on a fresh
    /// xHCI slot and a free device-table index, leaving the root hub the
    /// active control context.
    ///
    /// The shared attach core of the bring-up walk ([`Self::bring_up`]) and
    /// a hot-plug attach ([`Self::next_hub_change`]). The hub must already
    /// be installed; this rebinds EP0 to the free index's region,
    /// Enable-Slots the device, addresses it with the route string / TT for
    /// the downstream port, completes enumeration into the table entry,
    /// then restores the root hub as the active control context and clears
    /// the changes the attach latched. On failure the root hub is restored
    /// just the same and the enumerated slot, if any, is released — one
    /// port's broken device never leaves the engine wedged.
    ///
    /// A device that turns out to be a **hub** is installed into the hub
    /// table instead ([`AttachOutcome::Hub`], claiming the device region it
    /// was enumerated on) and descended: its ports are powered and scanned
    /// and its status-change watch armed ([`Self::descend_hub`]), so a hub
    /// plugged into a hub serves the devices behind it.
    ///
    /// A re-attach is a brand-new enumeration: a fresh slot, no reuse of any
    /// prior device state.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NoSpace`] if the device table is full.
    /// * [`DriverError::OutOfRange`] if the tier would exceed
    ///   [`MAX_HUB_DEPTH`] or the port cannot be route-encoded.
    /// * [`DriverError::DeviceFault`] if no hub is installed at
    ///   `hub_index`, the controller assigns no fresh slot, or any
    ///   command/transfer faults.
    /// * [`DriverError::BadMagic`] if a descriptor is forged.
    pub(crate) fn attach_downstream_device(
        &mut self,
        hub_index: usize,
        down_port: u8,
        speed: u8,
        delay: &dyn Delay,
    ) -> Result<AttachOutcome, DriverError> {
        let root_port = self
            .hub(hub_index)
            .ok_or(DriverError::DeviceFault)?
            .root_port;
        let index = self.claim_device_entry()?;
        let max_packet = ep0_max_packet(speed)?;

        self.rebind_to_device_region(index)?;
        let result = self.attach_on_rebound_region(
            index,
            Some((hub_index, down_port)),
            root_port,
            speed,
            max_packet,
        );
        // Rest the active control context again whether or not the attach
        // succeeded — no hub watch may lose its ring — and clear *every*
        // change this attach latched on the port: not just the connect
        // change, but the reset/enable changes `reset_hub_port` left set,
        // so the re-armed status-change watch fires only on the *next*
        // genuine hot-plug rather than immediately and forever on a stale
        // latch.
        let restored = self.rest_active_context();
        // Drain the latches on the failed path too: a failed attach that
        // leaves the connect/reset changes latched makes the hub's
        // status-change endpoint re-report the same stale change forever,
        // and each re-service re-runs the failing enumeration — the metal
        // symptom where one broken device pegs the hub watch in a
        // multi-second fault loop and starves every other port's service.
        let drained = if restored.is_ok() {
            self.hub_port_status_change(hub_index, down_port)
                .and_then(|(_, change)| self.clear_hub_port_changes(hub_index, down_port, change))
        } else {
            Ok(())
        };
        // Release every claim nothing owns — a failed attach's stranded
        // chunk(s), or the claimed entry of a child that turned out to be
        // served another way — so no attach outcome leaks DMA. On a clean
        // attach every claim is owned (the device's entry is live, or the
        // installed hub holds its region) and the sweep is a no-op.
        self.release_unattached_regions();
        let outcome = result?;
        restored?;
        drained?;
        // A freshly installed downstream hub is descended only now, with
        // the parent's latches drained and the resting context restored, so
        // its own attach failures can never wedge the parent's watch. A
        // tier that cannot be powered or watched is torn down whole rather
        // than left half-installed holding a slot and region.
        if let AttachOutcome::Hub(new_hub) = outcome {
            if let Err(err) = self.descend_hub(new_hub, delay) {
                let _ = self.detach_hub(new_hub);
                return Err(err);
            }
        }
        Ok(outcome)
    }

    /// The slot-level core of [`Self::attach_downstream_device`] and
    /// [`Self::attach_root_port`], run with the EP0 cursor already rebound
    /// to `index`'s region: Enable Slot, Address Device with the topology
    /// `parent` dictates (a downstream route string / TT behind a hub, the
    /// bare root topology on `root_port` otherwise), and complete
    /// enumeration into the table entry. A failure after the slot was
    /// assigned releases it (Disable Slot, DCBAA cleared, trailing events
    /// tolerated), so an aborted attach leaks nothing.
    ///
    /// A *transaction* fault (a USB or split transaction error) *while the
    /// control pipe is being established* ([`EnumStage::is_pipe_bringup`]) is a
    /// present-but-disturbed device, not a broken one, so enumeration re-drives
    /// a fresh slot up to [`ENUM_ATTEMPTS`] times before giving up (see there);
    /// a device that answers with an error (STALL/babble), a forged descriptor,
    /// or a fault past the pipe-bring-up phase is surfaced on the first attempt.
    fn attach_on_rebound_region(
        &mut self,
        index: usize,
        parent: Option<(usize, u8)>,
        root_port: u8,
        speed: u8,
        max_packet: u32,
    ) -> Result<AttachOutcome, DriverError> {
        let (route_string, tt_hub_slot, tt_port, parent_slot) = match parent {
            Some((hub_index, down_port)) => {
                let parent = self.hub(hub_index).ok_or(DriverError::DeviceFault)?;
                // The child extends its parent's Route String by one tier,
                // and a full/low-speed child routes through a transaction
                // translator: the parent's own when the parent is a
                // high-speed hub, else the one the parent itself inherited
                // (the nearest high-speed ancestor, §6.2.2 / §8.9). A
                // high-speed (or faster) child needs none.
                let route_string = route_for_child(parent.route_string, parent.depth, down_port)?;
                let (tt_hub_slot, tt_port) = if speed_needs_tt(speed) {
                    if parent.speed == SPEED_HIGH {
                        (parent.slot, down_port)
                    } else {
                        (parent.tt_hub_slot, parent.tt_port)
                    }
                } else {
                    (0, 0)
                };
                (route_string, tt_hub_slot, tt_port, parent.slot)
            }
            // A root attach: route string 0, no transaction translator.
            None => (0, 0, 0, 0),
        };
        let (hub_index, down_port) = parent.unwrap_or((0, 0));
        let base = SlotCtxBase {
            speed,
            root_port,
            route_string,
            tt_hub_slot,
            tt_port,
        };
        // Enable a slot, address the device, and complete enumeration —
        // re-driving a fresh slot on a *transaction* fault (a present-but-
        // disturbed device, see [`ENUM_ATTEMPTS`]). A fault that means the
        // device actively answered wrong (STALL/babble/forged descriptor) is
        // not retried; it re-fails deterministically and is surfaced at once.
        let mut attempt: u32 = 0;
        loop {
            self.stage = EnumStage::EnableSlot;
            let event = self.command(Trb::new(TrbType::EnableSlot, 0, 0, 0))?;
            let slot = event.slot_id();
            if slot == 0
                || slot > self.xhci.max_slots()
                || (parent.is_some() && slot == parent_slot)
            {
                return Err(DriverError::DeviceFault);
            }
            self.slot = slot;

            let attached = self
                .address_device(base, slot, max_packet)
                .and_then(|()| self.finish_enumeration(slot, base, index, down_port, hub_index))
                .and_then(|descriptor| {
                    if descriptor.is_hub() {
                        // The child is itself a hub: install it into the hub
                        // table, claiming the device region its contexts were
                        // enumerated on. The caller descends it after its
                        // latches are drained and the cursor rested.
                        self.install_hub(
                            parent.map(|(hub_index, _)| hub_index),
                            down_port,
                            base,
                            Some(index),
                        )
                        .map(AttachOutcome::Hub)
                    } else {
                        Ok(AttachOutcome::Device(index))
                    }
                });
            let err = match attached {
                Ok(outcome) => return Ok(outcome),
                Err(err) => err,
            };
            // Preserve the failure's live breadcrumb across the cleanup:
            // the Disable Slot below runs its own command wait, which
            // would overwrite the stage/completion/reject state a
            // capture needs to name the step that actually failed.
            let (stage, completion, event_type, reject) = (
                self.stage,
                self.last_completion,
                self.last_event_type,
                self.last_reject,
            );
            // Release the slot the failed attach claimed and tolerate any
            // trailing completion it still posts, so the shared event ring
            // consumers never fault on the aborted device.
            self.disable_slot_best_effort(slot);
            let _ = self.dma.write(
                self.layout.dcbaa + usize::from(slot) * 8,
                &0u64.to_le_bytes(),
            );
            self.tolerate_freed_slot(slot);
            self.stage = stage;
            self.last_completion = completion;
            self.last_event_type = event_type;
            self.last_reject = reject;

            // A transaction fault while the control pipe is still being
            // established leaves the device in its prior (Default) state — it
            // never saw the command — so a fresh slot re-drives it cleanly.
            // Retry a bounded number of times; a fault that means the device
            // answered wrong, one past the pipe-bring-up phase, or the final
            // attempt surfaces the error.
            attempt += 1;
            let transient = stage.is_pipe_bringup()
                && CompletionCode::from_raw(u32::from(completion))
                    .is_ok_and(CompletionCode::indicates_device_unreachable);
            if transient && attempt < ENUM_ATTEMPTS {
                continue;
            }
            return Err(err);
        }
    }

    /// Power, scan, and watch the freshly installed hub at `hub_index`:
    /// assert `PORT_POWER` on every downstream port, wait the
    /// power-on-good window, attach whatever is connected (recursing
    /// through further hub tiers via [`Self::attach_downstream_device`],
    /// bounded by [`MAX_HUB_DEPTH`] and the hub table), and arm the hub's
    /// status-change watch so later connects/disconnects on this tier
    /// arrive event-driven.
    ///
    /// The scan is per-port fail-soft, exactly like the bring-up walk: a
    /// port whose device fails enumeration is skipped with its latches
    /// drained, never costing the other ports their service. The watch is
    /// armed even when no port is connected, so the first later hot-plug
    /// is seen.
    ///
    /// # Errors
    ///
    /// [`DriverError`] from powering a port or arming the watch; a single
    /// port's failed attach is not an error.
    fn descend_hub(&mut self, hub_index: usize, delay: &dyn Delay) -> Result<(), DriverError> {
        let num_ports = self.hub(hub_index).ok_or(DriverError::NotFound)?.num_ports;
        for port in 1..=num_ports {
            self.power_hub_port(hub_index, port)?;
        }
        delay.delay_us(HUB_POWER_ON_GOOD_US);
        for port in 1..=num_ports {
            let Ok(status) = self.hub_port_status(hub_index, port) else {
                continue;
            };
            if !hub_port_connected(status) {
                continue;
            }
            // One broken or hostile device must not cost the other ports
            // their service: a failed attach releases its slot and chunks
            // inside `attach_hub_port` and the walk continues. A port the
            // bank can supply no memory for stays unserved fail-closed,
            // never displacing a served device. The skip is counted so the
            // driver can surface "present but unserved" instead of silence.
            if self.attach_hub_port(hub_index, port, delay).is_err() {
                self.skipped_ports = self.skipped_ports.saturating_add(1);
            }
        }
        self.configure_hub_watch(hub_index)
    }

    /// Record `slot` in the freed-slot tolerance set ([`Self::freed_slots`])
    /// so a trailing transfer completion for it is drained, never faulted
    /// on. When the set is full the oldest entry is replaced — its trailing
    /// events have had the longest window to arrive.
    fn tolerate_freed_slot(&mut self, slot: u8) {
        if slot == 0 || self.freed_slots.contains(&slot) {
            return;
        }
        if self.freed_slots.try_reserve(1).is_err() {
            // Bookkeeping-heap exhaustion: replace the oldest entry — its
            // trailing events have had the longest window to arrive —
            // rather than growing or failing the detach.
            if self.freed_slots.is_empty() {
                return;
            }
            self.freed_slots.remove(0);
        }
        self.freed_slots.push(slot);
    }

    /// Configure and arm the interrupt-IN status-change endpoint (USB 2.0
    /// §11.12.3) of the hub at `hub_index`, so a downstream
    /// connect/disconnect on that tier is delivered event-driven on the
    /// controller's event ring rather than polled. The shared ring is
    /// demultiplexed per endpoint ([`Self::hub_async_index`] /
    /// [`Self::report_async_index`]), so no two hubs' status reports — and
    /// no device report — ever collide.
    ///
    /// A no-op when the hub reported no status-change endpoint
    /// ([`HubState::int_endpoint`] is `None`): a hub that exposes none
    /// cannot be watched event-driven, so the engine runs without hotplug
    /// on that tier rather than failing bring-up. A spec-compliant hub
    /// always has one.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if no hub is live at `hub_index`.
    /// * [`DriverError`] from the Configure Endpoint command or the ring
    ///   build.
    pub(crate) fn configure_hub_watch(&mut self, hub_index: usize) -> Result<(), DriverError> {
        let hub = self.hub(hub_index).ok_or(DriverError::NotFound)?;
        let Some((dci, max_packet, interval)) = hub.int_endpoint else {
            return Ok(());
        };
        let (hub_slot, region, output_ctx) = (hub.slot, hub.region, hub.output_ctx);
        // Build the status-change endpoint's interrupt-IN transfer ring.
        let base = self.phys_of(region.int_ring)?;
        let (ring, link) = ProducerRing::new(RING_TRBS, base)?;
        self.dma.write(
            region.int_ring + ring.link_slot() * trb::TRB_LEN,
            &link.to_bytes(),
        )?;
        if let Some(hub) = self.hub_mut(hub_index) {
            hub.int_ring = Some(ring);
            hub.int_dci = dci;
        }

        // Configure Endpoint (A0 | A(dci)) adding the status-change endpoint
        // to the hub slot, copying the live slot context and raising its
        // Context Entries to cover the new DCI.
        let mut slot = self.read_ctx(output_ctx)?;
        slot[0] = (slot[0] & !SLOT_CTX_CONTEXT_ENTRIES_MASK)
            | (u32::from(dci) << SLOT_CTX_CONTEXT_ENTRIES_SHIFT);
        self.write_input_ctx(0, &input_control_dwords(1 | (1u32 << u32::from(dci))))?;
        self.write_input_ctx(1, &slot)?;
        self.write_input_ctx(
            1 + usize::from(dci),
            &ep_ctx_dwords(EP_TYPE_INTERRUPT_IN, max_packet, interval, base),
        )?;
        self.stage = EnumStage::ConfigureEndpoint;
        self.command(Trb::new(
            TrbType::ConfigureEndpoint,
            self.phys_of(self.layout.input_ctx)?,
            0,
            trb::control_slot(hub_slot),
        ))?;

        // Arm one status-change transfer and ring the hub's doorbell.
        self.arm_hub_report(hub_index)?;
        self.xhci.ring_doorbell(hub_slot, u32::from(dci))?;
        Ok(())
    }

    /// Prime one interrupt-IN transfer on the status-change endpoint of
    /// the hub at `hub_index` (a Normal TRB pointing at that hub's report
    /// buffer).
    fn arm_hub_report(&mut self, hub_index: usize) -> Result<(), DriverError> {
        let region = self.hub(hub_index).ok_or(DriverError::DeviceFault)?.region;
        let buffer = self.phys_of(region.report)?;
        let report_len =
            u32::try_from(HUB_REPORT_LEN).map_err(|_| DriverError::LengthOutOfRange)?;
        let normal = Trb::new(
            TrbType::Normal,
            buffer,
            report_len,
            trb::CONTROL_IOC | trb::CONTROL_ISP,
        );
        let ring = self
            .hub_mut(hub_index)
            .and_then(|hub| hub.int_ring.as_mut())
            .ok_or(DriverError::DeviceFault)?;
        let outcome = ring.push(normal)?;
        let link_slot = ring.link_slot();
        publish(&mut self.dma, region.int_ring, link_slot, &outcome)
    }

    /// Issue a Disable Slot command for `slot` (xHCI §6.4.3.3) **best-effort**,
    /// returning the slot to the controller's pool if the controller confirms.
    ///
    /// A device-removal teardown must complete locally even when the gone
    /// device's hub cannot let the controller post the Disable Slot completion
    /// in time (the metal failure: the confirmation times out and the slot was
    /// never freed, so a re-plug was never re-enumerated). So this never fails
    /// the teardown: it posts the command, waits within budget, and retires the
    /// command-ring slot whether or not the completion was observed — keeping
    /// the command ring consistent for the next enumeration. A late completion
    /// is drained as a freed-slot event by the event-ring consumers.
    fn disable_slot_best_effort(&mut self, slot: u8) {
        self.reset_event_diagnostics();
        let command = Trb::new(TrbType::DisableSlot, 0, 0, trb::control_slot(slot));
        let Ok(outcome) = self.command_ring.push(command) else {
            return;
        };
        if publish(
            &mut self.dma,
            self.layout.command_ring,
            self.command_ring.link_slot(),
            &outcome,
        )
        .is_err()
            || self.xhci.ring_doorbell(0, 0).is_err()
        {
            let _ = self.command_ring.retire_one();
            return;
        }
        // Wait within budget for the Disable Slot completion so the command
        // ring is left consistent for the next enumeration; a late completion
        // is drained as a freed-slot event by the event-ring consumers instead.
        let _ = self.await_event_for(&[outcome.address]);
        // Retire our producer slot regardless: a removed device's teardown must
        // not leave the command ring wedged, and any late completion is drained
        // as a freed-slot event rather than retired here a second time.
        let _ = self.command_ring.retire_one();
    }

    /// Tear down the served device at `index` after it has disconnected:
    /// Disable its slot, clear its DCBAA entry, and drop its table entry —
    /// **and every sibling entry sharing its slot**, since a composite
    /// device's interfaces vanish together with the physical device — with
    /// all per-device state (so a re-attach is a brand-new enumeration; the
    /// fresh attach rebuilds its rings in the regions). The hub stays
    /// addressed and watched, and every other served device is untouched.
    ///
    /// # Errors
    ///
    /// [`DriverError`] from the local DMA write that clears the DCBAA entry.
    /// The Disable Slot command is best-effort and never fails the teardown
    /// (see [`Self::disable_slot_best_effort`]).
    fn detach_device(&mut self, index: usize) -> Result<(), DriverError> {
        let Some(slot) = self
            .devices
            .get(index)
            .and_then(Option::as_ref)
            .map(|device| device.slot)
        else {
            return Ok(());
        };
        let mut lost_active = false;
        for entry_index in 0..self.devices.len() {
            let shares_slot = self.devices[entry_index]
                .as_ref()
                .is_some_and(|device| device.slot == slot);
            if !shares_slot {
                continue;
            }
            if self.active_device == Some(entry_index) {
                // The vanished device is the active control context; clear
                // the cursor index now (its ring is dropped with the
                // device) and re-rest the cursor after the entries are
                // gone, so the rest target can never be the freed entry
                // itself.
                self.active_device = None;
                lost_active = true;
            }
            self.devices[entry_index] = None;
            // Return the entry's DMA chunk to the bank; a re-attach is a
            // brand-new enumeration on a brand-new chunk.
            self.release_device_region(entry_index);
        }
        if lost_active {
            // Best-effort: the cursor must land somewhere safe (another
            // live entry, or the idle layout binding), but a failed rest
            // never fails the teardown.
            let _ = self.rest_active_context();
        }
        if slot != 0 {
            self.disable_slot_best_effort(slot);
            self.dma.write(
                self.layout.dcbaa + usize::from(slot) * 8,
                &0u64.to_le_bytes(),
            )?;
            // Tolerate a trailing transfer completion the controller may
            // still post for this now-gone slot (a dropped in-flight
            // transfer, or a Disable Slot side-effect), so the event-ring
            // consumers drain it instead of faulting the hub watch on it.
            // Cleared again once a fresh device enumerates.
            self.tolerate_freed_slot(slot);
        }
        Ok(())
    }

    /// Tear down the hub at `hub_index` and **everything behind it** after
    /// it has disconnected: every served device on its downstream ports,
    /// every child hub (recursively — unplugging a hub takes each deeper
    /// tier with it), and finally the hub's own slot, watch ring, and
    /// claimed device region. Every other hub and served device is
    /// untouched, and the depth is bounded by [`MAX_HUB_DEPTH`].
    ///
    /// # Errors
    ///
    /// [`DriverError`] from the local DMA writes that clear the DCBAA
    /// entries; the Disable Slot commands are best-effort as in
    /// [`Self::detach_device`].
    fn detach_hub(&mut self, hub_index: usize) -> Result<(), DriverError> {
        if self.hub(hub_index).is_none() {
            return Ok(());
        }
        for index in 0..self.devices.len() {
            let behind = self.devices[index]
                .as_ref()
                .is_some_and(|device| device.hub_port != 0 && device.parent_hub == hub_index);
            if behind {
                self.detach_device(index)?;
            }
        }
        for child in 0..self.hubs.len() {
            let behind = self
                .hub(child)
                .is_some_and(|hub| hub.parent == Some(hub_index));
            if behind {
                self.detach_hub(child)?;
            }
        }
        let lost_active = self.active_hub == Some(hub_index);
        if lost_active {
            // The vanished hub is the active control context; clear the
            // cursor index now (its ring is dropped with the entry) and
            // re-rest after the entry is gone, so the rest target can
            // never be the freed hub itself.
            self.active_hub = None;
        }
        let Some(hub) = self.hubs[hub_index].take() else {
            return Ok(());
        };
        if lost_active {
            // Best-effort, exactly as in `detach_device`.
            let _ = self.rest_active_context();
        }
        // Return the hub's status-change watch chunk — and the device-region
        // chunk its contexts were enumerated on — to the bank.
        let _ = self.dma.release(hub.region.base);
        if let Some(region_index) = hub.device_region {
            self.release_device_region(region_index);
        }
        self.disable_slot_best_effort(hub.slot);
        self.dma.write(
            self.layout.dcbaa + usize::from(hub.slot) * 8,
            &0u64.to_le_bytes(),
        )?;
        // Tolerate a trailing completion for the hub's own slot (an armed
        // status-change transfer dropped by the unplug), exactly as for a
        // device slot.
        self.tolerate_freed_slot(hub.slot);
        Ok(())
    }

    /// Whether this engine is watching at least one hub's status-change
    /// endpoint event-driven (a hub is addressed and its endpoint armed).
    #[must_use]
    pub fn hub_watch_active(&self) -> bool {
        self.hubs
            .iter()
            .any(|entry| entry.as_ref().is_some_and(|hub| hub.int_ring.is_some()))
    }

    /// Hubs whose status-change endpoint is watched — the most distinct reports
    /// one ring drain can have parked, since each hub keeps exactly one status
    /// transfer outstanding.
    ///
    /// This bounds the caller's service loop: draining every parked report in
    /// one pass is what stops a second reporting hub being stranded with no
    /// outstanding transfer to raise the next interrupt, while the bound stops
    /// a *flapping* hub — whose re-armed endpoint completes again during each
    /// service — from holding the loop and starving the other devices' URBs.
    #[must_use]
    pub fn watched_hub_count(&self) -> usize {
        self.hubs
            .iter()
            .filter(|entry| entry.as_ref().is_some_and(|hub| hub.int_ring.is_some()))
            .count()
    }

    /// Confirm and detach the served device at `index` after its interrupt
    /// or bulk endpoint faulted.
    ///
    /// Some controllers report a physical unplug first as a failed transfer on
    /// the device's endpoint, before the hub status-change endpoint posts its
    /// own completion. The HCD calls this only from that event-driven fault
    /// path.
    ///
    /// The device's *own* interrupt-IN endpoint may already have reported a
    /// completion code that is conclusive on its own — the device failed to
    /// answer a transaction, i.e. it is unreachable
    /// ([`CompletionCode::indicates_device_unreachable`], captured in the
    /// device's `last_report_fault_code`). On a low/full-speed keyboard behind
    /// a high-speed hub's transaction translator a hot-removal surfaces as a
    /// Split Transaction Error there, and the gone device's hub frequently
    /// cannot answer a `GET_PORT_STATUS` confirmation in time. So when the fault
    /// code is a device-unreachable code the slot is freed directly, without
    /// depending on the unreliable hub control transfer.
    ///
    /// Otherwise — a fault code that is not conclusive of removal — it falls
    /// back to reading the port the device hangs off: a hub-downstream
    /// device's parent hub port (a class `GET_PORT_STATUS`), a
    /// directly-attached device's root port (a `PORTSC` register read).
    /// Only if the port now reports disconnected is the device freed; a
    /// live device's ordinary transfer fault is left visible to the
    /// caller. Either way the port's connection-change latch is left for
    /// its watcher (the hub's status-change endpoint, or the root-port
    /// scan [`Self::next_root_change`]) to report and drain, so a later
    /// reconnect is still seen.
    ///
    /// # Errors
    ///
    /// [`DriverError`] from the hub control transfer or slot teardown.
    pub fn detach_if_device_gone(&mut self, index: usize) -> Result<bool, DriverError> {
        let Some(device) = self.device(index) else {
            return Ok(false);
        };
        let port = device.hub_port;
        let root_port = device.root_port;
        let parent_hub = device.parent_hub;
        let fault_code = device.last_report_fault_code;
        if port != 0 && self.hub(parent_hub).is_none() {
            return Ok(false);
        }
        // The device's own endpoint already gave a conclusive device-gone
        // verdict; free the slot directly rather than trusting a confirmation
        // the vanished device's hub often cannot answer.
        if CompletionCode::from_raw(u32::from(fault_code))
            .is_ok_and(CompletionCode::indicates_device_unreachable)
        {
            self.detach_device(index)?;
            return Ok(true);
        }
        let connected = if port == 0 {
            // Directly attached: the root port's live connect bit is the
            // confirmation. A read fault is treated as still-connected, so
            // a transient register fault never triggers a spurious
            // teardown (fail safe).
            if root_port == 0 {
                return Ok(false);
            }
            self.xhci
                .port_status(root_port)
                .map_or(true, PortStatus::connected)
        } else {
            let (status, _change) = self.hub_port_status_change(parent_hub, port)?;
            hub_port_connected(status)
        };
        if connected {
            return Ok(false);
        }
        self.detach_device(index)?;
        Ok(true)
    }

    /// Service one hub status-change notification — from whichever watched
    /// hub tier reported it — returning what changed.
    ///
    /// Called by the HCD when the controller interrupt fires while a hub is
    /// watched ([`Self::hub_watch_active`]): it takes the status-change
    /// completion the shared ring drain parked for that hub — the same drain
    /// [`Self::pump_reports`] and every synchronous wait run —
    /// reads the changed downstream port on the reporting hub, and either
    /// enumerates a freshly connected device ([`HubEvent::Attached`], a
    /// brand-new enumeration — or a fresh hub tier,
    /// [`HubEvent::HubAttached`], installed, descended, and watched) or
    /// frees a disconnected one ([`HubEvent::Detached`]; an unplugged hub
    /// cascades into [`HubEvent::HubDetached`], freeing every device and
    /// tier behind it). The status-change transfer is re-armed for the next
    /// change. Entirely event-driven — it neither polls nor spins; with no
    /// completion pending it returns [`HubEvent::None`].
    ///
    /// `delay` supplies the downstream-port reset-recovery window on a fresh
    /// connect; the caller owns the clock.
    ///
    /// # Errors
    ///
    /// [`DriverError`] from a control/command transfer (fail closed); the
    /// status-change transfer is re-armed before returning so a single odd
    /// report never silences the watch.
    pub fn next_hub_change(&mut self, delay: &dyn Delay) -> Result<HubEvent, DriverError> {
        if !self.hub_watch_active() {
            return Ok(HubEvent::None);
        }
        // A fresh service gets a fresh fault snapshot: whatever this call
        // surfaces is what [`Self::last_attach_fault`] then describes.
        self.attach_fault = None;
        // Drain through the one shared classifier, which parks each watched
        // hub's status-change completion (and captures reports, parks bulk,
        // arms the root-port scan) — then take what it parked. A second
        // hub-specific ring walk beside it would be a duplicate of that
        // dispatch decision, and the walk it replaced read the whole event
        // segment per poll.
        self.drain_events()?;
        let Some(hub_index) = self.take_parked_hub_completion() else {
            return Ok(HubEvent::None);
        };
        if let Some(ring) = self
            .hub_mut(hub_index)
            .and_then(|hub| hub.int_ring.as_mut())
        {
            ring.retire_one()?;
        }
        // Service the change, but re-arm the status-change endpoint
        // **regardless of the outcome**. Right after a downstream disconnect
        // the gone device's transaction translator can briefly fail to answer
        // the hub's `GET_PORT_STATUS`, so servicing this report errors; if the
        // re-arm were skipped on that error the status-change endpoint would be
        // left with no outstanding transfer and the hub could never post
        // another report — the later reconnect would then produce no interrupt
        // and go unseen. Re-arming first keeps the watch live so a single odd
        // report never silences it; the error is surfaced afterwards.
        let outcome = self.process_hub_change(hub_index, delay);
        // The serviced hub can only have *survived* its own report (it never
        // detaches itself), so the re-arm targets a live watch.
        self.arm_hub_report(hub_index)?;
        let (slot, dci) = self
            .hub(hub_index)
            .map(|hub| (hub.slot, hub.int_dci))
            .ok_or(DriverError::NotFound)?;
        self.xhci.ring_doorbell(slot, u32::from(dci))?;
        outcome
    }

    /// Take the first hub with a status-change completion parked by a
    /// synchronous wait ([`Self::stash_async_event`]), returning its index.
    fn take_parked_hub_completion(&mut self) -> Option<usize> {
        self.hubs.iter_mut().position(|entry| {
            entry
                .as_mut()
                .is_some_and(|hub| hub.pending.take().is_some())
        })
    }

    /// Read the reporting hub's port-change bitmap and act on the first
    /// changed downstream port: enumerate a freshly connected device
    /// ([`HubEvent::Attached`] — or a fresh hub tier,
    /// [`HubEvent::HubAttached`], installed, descended, and watched) or
    /// free what was served on a disconnected port ([`HubEvent::Detached`];
    /// an unplugged hub cascades into [`HubEvent::HubDetached`]).
    ///
    /// Every changed port goes through the shared per-port decision
    /// ([`Self::reconcile_hub_port`]), which drains the port's **whole**
    /// latched change set — not just the connect change — so the
    /// status-change watch re-arms clean and never wedges firing forever on
    /// a stale reset/enable change. A change that is not a
    /// connect/disconnect we act on (a reset or enable change, a connect
    /// for a port already served, or a connect with the device table full)
    /// is drained and ignored.
    ///
    /// The scan is **per-port fail-soft**, mirroring the bring-up walk: one
    /// port's broken or unresponsive device has its latches drained
    /// ([`Self::attach_hub_port`]) and the remaining changed ports are still
    /// serviced, so a mouse that fails enumeration can never cost the
    /// keyboard beside it its hot-plug. The first failure is surfaced (the
    /// caller logs it) only when no actionable event was found.
    fn process_hub_change(
        &mut self,
        hub_index: usize,
        delay: &dyn Delay,
    ) -> Result<HubEvent, DriverError> {
        let hub = self.hub(hub_index).ok_or(DriverError::NotFound)?;
        let (num_ports, report_off) = (hub.num_ports, hub.region.report);
        let mut bitmap = [0u8; HUB_REPORT_LEN];
        self.dma.read(report_off, &mut bitmap)?;
        let mut first_failure = None;
        for port in 1..=num_ports {
            let byte = usize::from(port / 8);
            let bit = port % 8;
            if byte >= HUB_REPORT_LEN || bitmap[byte] & (1 << bit) == 0 {
                continue;
            }
            match self.reconcile_hub_port(hub_index, port, delay) {
                Ok(Some(event)) => return Ok(event),
                Ok(None) => {}
                Err(err) => {
                    if first_failure.is_none() {
                        first_failure = Some(err);
                    }
                }
            }
        }
        match first_failure {
            Some(err) => Err(err),
            None => Ok(HubEvent::None),
        }
    }

    /// Reconcile one downstream port's **live** state against the tracking
    /// tables, taking whatever topology action the state demands.
    ///
    /// The single per-port hot-plug decision the status-change service
    /// ([`Self::process_hub_change`]) drives every changed port through. It
    /// is keyed on the port's *current* `GET_PORT_STATUS` state compared
    /// with what the engine tracks — never on the latched change bits alone
    /// — because a latch can be stale (a change already acted on, or
    /// drained by an earlier teardown) while the state is real:
    ///
    /// * A device present on a port with nothing tracked is enumerated as
    ///   brand-new (or installed, descended, and watched as a fresh hub
    ///   tier). [`Self::attach_hub_port`] resets the port and drains every
    ///   latch (including any connect change) whether or not it succeeds.
    /// * Nothing on a port the engine tracks something on: the latches are
    ///   drained and the tracked device — or hub, with every device and
    ///   deeper tier behind it in one cascade — is freed. Every other
    ///   served device and hub is untouched.
    /// * Any other state (a latch with no topology action — a
    ///   reset/enable/suspend/over-current change, or a connect for a port
    ///   already served): the latches are drained so the status-change
    ///   watch re-arms clean rather than re-firing on the stale change.
    ///
    /// Returns the event taken, or `None` when the port needed no action.
    ///
    /// # Errors
    ///
    /// [`DriverError`] from the port-status read, the latch drain, or the
    /// attach/detach (fail closed; the callers are per-port fail-soft).
    fn reconcile_hub_port(
        &mut self,
        hub_index: usize,
        port: u8,
        delay: &dyn Delay,
    ) -> Result<Option<HubEvent>, DriverError> {
        let (status, change) = self.hub_port_status_change(hub_index, port)?;
        if hub_port_connected(status)
            && self
                .device_index_for_hub_and_port(hub_index, port)
                .is_none()
            && self.hub_index_for_hub_and_port(hub_index, port).is_none()
        {
            return match self.attach_hub_port(hub_index, port, delay) {
                Ok(AttachOutcome::Device(index)) => Ok(Some(HubEvent::Attached(index))),
                Ok(AttachOutcome::Hub(new_hub)) => Ok(Some(HubEvent::HubAttached(new_hub))),
                Err(err) => Err(err),
            };
        }
        if !hub_port_connected(status) {
            if let Some(index) = self.device_index_for_hub_and_port(hub_index, port) {
                self.clear_hub_port_changes(hub_index, port, change)?;
                self.detach_device(index)?;
                return Ok(Some(HubEvent::Detached(index)));
            }
            if let Some(child) = self.hub_index_for_hub_and_port(hub_index, port) {
                self.clear_hub_port_changes(hub_index, port, change)?;
                self.detach_hub(child)?;
                return Ok(Some(HubEvent::HubDetached(child)));
            }
        }
        if change != 0 {
            self.clear_hub_port_changes(hub_index, port, change)?;
        }
        Ok(None)
    }

    /// Reset the controller and re-enumerate from scratch, treating whatever
    /// is now attached as brand-new devices.
    ///
    /// The recovery path for a root-port (re)connect — both the first
    /// cold-boot attach when nothing was present at bring-up and a
    /// disconnect→reconnect: a full Host Controller Reset clears every slot,
    /// address, and context the controller held, then the held register
    /// window and DMA region are re-programmed and the whole bring-up walk
    /// re-runs. No prior device state is reused, so every (re)attached
    /// device is treated as brand-new. (Hub-downstream hotplug uses the
    /// finer-grained [`Self::next_hub_change`] instead, leaving the
    /// controller running.)
    ///
    /// `delay` supplies the enumeration settle windows; the caller owns the
    /// clock. Afterwards the served devices are the live
    /// [`Self::device_live`] indices — none, if everything had already gone
    /// again by the time the controller came back (no spurious failure).
    ///
    /// # Errors
    ///
    /// [`DriverError`] from the controller reset, re-programming, or
    /// bring-up (fail closed).
    pub fn reset_and_reenumerate(&mut self, delay: &dyn Delay) -> Result<(), DriverError> {
        self.xhci
            .reset_to_ready(self.budget)
            .map_err(|err| err.error)?;
        let layout = self.layout;
        let (command_ring, ep0_ring, event_cursor) =
            Self::program_and_start(&mut self.xhci, &mut self.dma, &layout, self.budget)?;
        self.command_ring = command_ring;
        self.ep0_ring = ep0_ring;
        self.event_cursor = event_cursor;
        // The reset cleared every `PORTSC` latch and the fresh ring has posted
        // no Port Status Change Event yet, so nothing would arm the scan for
        // ports the controller comes back with already connected.
        self.root_change_pending = true;
        self.reset_device_tracking();
        self.stage = EnumStage::Scan;
        self.reset_event_diagnostics();
        self.bring_up(delay)
    }

    /// The enumeration step the most recent attach last entered.
    ///
    /// After a [`Self::bring_up`] failure this pins
    /// which xHCI operation a [`DriverError::DeviceFault`] came from —
    /// [`EnumStage::Scan`] means no connected port was ever entered (an
    /// empty hub / [`DriverError::NotFound`]); any later variant names
    /// the faulting step.
    #[must_use]
    pub const fn enum_stage(&self) -> EnumStage {
        self.stage
    }

    /// Downstream hub ports whose connected device failed enumeration and
    /// was skipped fail-soft by the most recent bring-up walk
    /// ([`Self::bring_up`] / [`Self::reset_and_reenumerate`]), so the
    /// driver can log "a device was present but never served" rather than
    /// the port silently looking empty.
    #[must_use]
    pub const fn skipped_port_count(&self) -> u32 {
        self.skipped_ports
    }

    /// The first failed downstream-port attach of the most recent service
    /// ([`Self::next_hub_change`]) or bring-up walk — the port, stage, and
    /// controller/hub state snapshotted at the failure, before the failure
    /// path's own cleanup transfers overwrote the live diagnostics. `None`
    /// when every attach of that service succeeded (or none ran).
    #[must_use]
    pub const fn last_attach_fault(&self) -> Option<AttachFault> {
        self.attach_fault
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

    /// Raw completion code of the most recent interrupt-IN report the engine
    /// rejected for the device at `index` (`0` = none rejected since its
    /// attach, or no device live there).
    ///
    /// This is the controller's verdict on the device's *own* endpoint at a
    /// hot-removal, captured when an interrupt-IN report is rejected and —
    /// unlike [`Self::last_completion_code`] — not overwritten by the hub
    /// disconnect-confirmation control transfer that follows it. It tells a
    /// metal capture whether the unplug surfaced as a transient transaction
    /// error or a definitive device-gone / stall code.
    #[must_use]
    pub fn last_report_fault_code(&self, index: usize) -> u8 {
        self.device(index)
            .map_or(0, |device| device.last_report_fault_code)
    }

    /// The HID enumeration decision for the interface at `index`, or `None`
    /// when no device is live there or the interface carries no interrupt-IN
    /// endpoint (a bulk interface has no report path to diagnose).
    ///
    /// A one-shot diagnostic the host-controller driver logs when it publishes
    /// the interface node, so a metal capture shows whether the device runs in
    /// report or boot protocol, the parsed field layout, and the armed
    /// transfer size — the only window on the live path (QEMU models no Pi
    /// USB). It mints no resources and takes no capability.
    #[must_use]
    pub fn hid_enum_diag(&self, index: usize) -> Option<HidEnumDiag> {
        let device = self.device(index)?;
        if device.int_dci == DCI_CONTROL || device.int_ring.is_none() {
            return None;
        }
        let capture_len = u16::try_from(device.int_capture_len()).unwrap_or(u16::MAX);
        Some(HidEnumDiag {
            report_descriptor_len: device.report_descriptor_len,
            report_protocol: matches!(device.report_decode, ReportDecode::Mapped(_)),
            reports_refused: matches!(device.report_decode, ReportDecode::Refused),
            map: device.report_decode.map().map(|map| map.summary()),
            int_max_packet: device.int_max_packet,
            capture_len,
        })
    }

    /// The Report Descriptor bytes interface `index` delivered, empty when it
    /// declared none or refused the read.
    ///
    /// The companion of [`Self::hid_enum_diag`]: the diagnostic reports what
    /// the map *says*, this reports what it was derived *from*, which is the
    /// only way a mis-parse is diagnosable on hardware QEMU cannot model. A
    /// Report Descriptor is a device capability blob and carries no report
    /// payload, so a log of it discloses no keystroke or pointer movement.
    #[must_use]
    pub fn hid_report_descriptor(&self, index: usize) -> &[u8] {
        self.device(index)
            .map_or(&[], |device| device.report_descriptor.as_bytes())
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

    /// Whether the controller has latched a fatal error or halted
    /// (delegates to [`Xhci::controller_faulted`]).
    ///
    /// A faulted controller raises no further interrupts until it is reset, so
    /// a downstream device's hot-plug and transfers go silent. The Pi 4 VL805
    /// latches a Host System Error during a downstream-device hot-removal
    /// teardown (after its Disable Slot completes), so the HCD checks this
    /// after servicing a wake and recovers with [`Self::reset_and_reenumerate`]
    /// — the same full Host Controller Reset and fresh enumeration a cold boot
    /// with no device attached performs, returning to the proven await-connect
    /// state so a re-plug enumerates normally.
    #[must_use]
    pub fn controller_faulted(&mut self) -> bool {
        self.xhci.controller_faulted()
    }

    /// Raw `PORTSC` of root-hub `port` (1-based) for a bring-up diagnostic,
    /// or `None` if the port is out of range or the read faults. A capture
    /// of the connect/power/enable/speed bits when enumeration stalls on a
    /// root port.
    pub fn port_status_raw(&mut self, port: u8) -> Option<u32> {
        self.xhci.port_status(port).ok().map(crate::PortStatus::raw)
    }

    /// Describe the served device at `index` as a discovered child
    /// [`HwNode`] parented at `parent_id` and assigned `node_id`.
    ///
    /// The node carries one [`HwMatchKey::usb`] of the device's
    /// `vid:pid` and the 24-bit class of the interface this driver
    /// brought up — both read from the device during enumeration, never
    /// assumed — so `devmgr` resolves a class driver's signed bind table
    /// against it. Its [`HwDeviceClass`] is derived from the interface
    /// class, the match key mirroring the PCI child node
    /// [`PciBus::describe_function`](tairix_abi::driver::pci::PciBus::describe_function)
    /// emits for the controller above it. The node's device address is the
    /// device's xHCI slot id, so every interface node of one composite
    /// device carries the same non-zero address (a slot is never `0` while
    /// served) and an inventory consumer can attribute sibling interfaces
    /// to their one physical device.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] if no device is live at `index` (the
    ///   identity is captured only on a successful enumeration) — fail
    ///   closed, never a fabricated node.
    /// * [`DriverError::DeviceFault`] if the match key cannot be pushed.
    ///
    /// # Capabilities
    ///
    /// None — describing a node mints no resources (:
    /// resources are minted at the load gate).
    pub fn describe_device(
        &self,
        index: usize,
        parent_id: u32,
        node_id: u32,
    ) -> Result<HwNode, DriverError> {
        let device = self.device(index).ok_or(DriverError::NotFound)?;
        let identity = device.identity;
        let slot = device.slot;
        // Derive the node's device class from the interface's own class
        // byte, never assumed: a HID interface is an input device, a
        // mass-storage interface a storage device. An unmapped class is
        // honestly `Other` — the match keys still carry the exact triple.
        let device_class = match identity.interface_class >> 16 {
            0x03 => HwDeviceClass::Input,
            0x08 => HwDeviceClass::Storage,
            _ => HwDeviceClass::Other,
        };
        let mut node = HwNode::new(node_id, parent_id, device_class);
        node.set_address(u32::from(slot));
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
impl<H: XhciHost, M: DmaBank> UsbDevice<'_, H, M> {
    /// Test-only access to the register seam, so the crate's unit
    /// tests can drive and assert the mock controller's state.
    pub(crate) fn host_mut(&mut self) -> &mut H {
        &mut self.xhci.host
    }

    /// Test-only access to the DMA bank, so the cost-budget regression can read
    /// the mock bank's access counters.
    pub(crate) fn dma_mut(&mut self) -> &mut M {
        &mut self.dma
    }

    /// Test-only read of a served device's raw slot, so a hot-removal test
    /// can capture which slot a later trailing transfer event names.
    pub(crate) fn raw_device_slot(&self, index: usize) -> u8 {
        self.device(index).map_or(0, |device| device.slot)
    }

    /// Test-only read of the active control-context slot, so a hub-descent
    /// test can assert which slot the hub occupies.
    pub(crate) fn active_slot(&self) -> u8 {
        self.slot
    }

    /// Test-only read of a served device's captured identity, so a test can
    /// assert the enumerated `vid:pid:class` without a node round-trip.
    pub(crate) fn device_identity(&self, index: usize) -> Option<DeviceIdentity> {
        self.device(index).map(|device| device.identity)
    }

    /// Test-only view of the DMA bank, so a test can observe its chunk
    /// accounting (a region allocated on attach, released on detach).
    pub(crate) fn dma_ref(&self) -> &M {
        &self.dma
    }

    /// Test-only raw command issue, so the wait-timeout regression can
    /// drive one synchronous completion wait directly.
    pub(crate) fn command_for_test(&mut self, command: Trb) -> Result<Trb, DriverError> {
        self.command(command)
    }
}

impl<H: XhciHost, M: DmaBank> UsbDevice<'_, H, M> {
    /// Decode one completed interrupt-IN [`TrbType::TransferEvent`] (already
    /// confirmed to target device `index`'s slot and interrupt endpoint)
    /// into a report length, copying the report bytes into `buf`.
    ///
    /// This performs only the *validation and copy* of one transfer; it does
    /// **not** touch the transfer ring. Re-arming the endpoint is the caller's
    /// (`next_report`) unconditional responsibility, so that a transfer whose
    /// completion code or buffer mapping this method rejects still leaves the
    /// endpoint re-armed for the next report (a single odd transfer must never
    /// silence the keyboard).
    ///
    /// `Ok(Some(len))` is a delivered report of `len` bytes; `Ok(None)` is a
    /// *successful* transfer that carried **zero** bytes (a zero-length
    /// packet — a residual equal to the whole request). A ZLP is not a report
    /// and not a fault: a composite/idle HID interface (a wireless MMO mouse's
    /// extra collection) legitimately completes an interrupt-IN transfer with
    /// no data. The caller re-arms and keeps the URB parked rather than
    /// replying, so an idle or ZLP-streaming device neither spins the class
    /// driver on empty completions nor is killed by a false fault.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] for an unexpected completion code, a
    /// completed-TRB address outside the interrupt ring, a misaligned or
    /// out-of-range ring slot, or a residual larger than the report.
    fn decode_transfer_report(
        &mut self,
        index: usize,
        event: Trb,
        buf: &mut [u8],
    ) -> Result<Option<usize>, DriverError> {
        let region = self.device(index).ok_or(DriverError::NotFound)?.region;
        let ring_base = self.phys_of(region.int_ring)?;
        let device = self.device_mut(index).ok_or(DriverError::NotFound)?;
        if !matches!(
            event.completion_code(),
            Ok(CompletionCode::Success | CompletionCode::ShortPacket)
        ) {
            // Preserve the controller's verdict on the device's own
            // interrupt-IN endpoint before failing closed: a later hub
            // disconnect-confirmation control transfer resets the shared event
            // diagnostics, so this is the only surviving record of why the
            // report faulted (a transient transaction error vs. a device-gone /
            // stall code).
            device.last_report_fault_code = event.completion_code_raw();
            return Err(DriverError::DeviceFault);
        }
        // Map the completed TRB back to its slot's report buffer,
        // validating every step of the controller's claim.
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
        // The transfer was armed to the endpoint's packet size; the residual is
        // the tail the device left untransferred, so the delivered report is
        // the difference.
        let len = device
            .int_capture_len()
            .checked_sub(residual)
            .ok_or(DriverError::DeviceFault)?;
        if len > buf.len() {
            return Err(DriverError::DeviceFault);
        }
        // A zero-length completion is a successful transfer that carried no
        // report; it is neither delivered nor a fault (the caller re-arms and
        // parks). Nothing is copied out.
        if len == 0 {
            return Ok(None);
        }
        self.dma
            .read(region.report_bufs + slot * CAPTURE_LEN, &mut buf[..len])?;
        Ok(Some(len))
    }

    /// Retire device `index`'s just-completed interrupt-IN transfer.
    ///
    /// Called by [`Self::next_report`] for **every** completed transfer
    /// event addressed to that endpoint — including one whose report was
    /// rejected by [`Self::decode_transfer_report`] — so the transfer-ring
    /// software dequeue always matches what the controller has consumed.
    /// Retiring frees one ring slot, which [`Self::ensure_reports_armed`]
    /// immediately re-arms, so the endpoint is kept armed to
    /// [`INT_ARM_DEPTH`] and the controller always has a landing TRB for the
    /// next report rather than going idle between class-driver URBs.
    ///
    /// # Errors
    ///
    /// [`DriverError::OutOfRange`] if the controller reported a completion
    /// when no transfer was in flight.
    fn retire_interrupt_transfer(&mut self, index: usize) -> Result<(), DriverError> {
        self.device_mut(index)
            .ok_or(DriverError::NotFound)?
            .int_ring
            .as_mut()
            .ok_or(DriverError::DeviceFault)?
            .retire_one()
    }

    /// Recover device `index`'s halted interrupt-IN endpoint after a transfer
    /// completed with a halting error code that left the device present (a
    /// STALL, babble, data-buffer or TRB error): Reset Endpoint (§4.6.8),
    /// rebuild the transfer ring at its base and repoint the controller's
    /// dequeue there (§4.6.10), then clear the device-side halt so its data
    /// toggle resets (USB 2.0 §9.4.5). The endpoint accepts fresh interrupt
    /// transfers when this returns.
    ///
    /// A halted endpoint is left permanently stopped by the controller until
    /// it is reset; re-arming it without this recovery makes every re-armed
    /// transfer re-fault — a controller interrupt storm — or, once the class
    /// driver stops retrying, silences the device. This is the interrupt-IN
    /// counterpart of [`Self::recover_bulk_endpoint`].
    ///
    /// # Errors
    ///
    /// [`DriverError`] from a Reset Endpoint / Set TR Dequeue Pointer command,
    /// the ring rebuild, or the device-side `CLEAR_FEATURE` — surfaced so a
    /// device that vanished mid-recovery still tears down fail-closed.
    fn recover_interrupt_endpoint(&mut self, index: usize) -> Result<(), DriverError> {
        let (int_ring_off, dci, device_slot) = {
            let device = self.device(index).ok_or(DriverError::NotFound)?;
            (device.region.int_ring, device.int_dci, device.slot)
        };
        // Reset Endpoint clears the controller-side halt.
        self.command(Trb::new(
            TrbType::ResetEndpoint,
            0,
            0,
            trb::control_slot(device_slot) | trb::control_endpoint(dci),
        ))?;
        // Rebuild the software ring at its base and clear any abandoned TRBs,
        // then point the controller's dequeue at the fresh base with Dequeue
        // Cycle State 1 to match.
        let zeros = [0u8; trb::TRB_LEN];
        for ring_slot in 0..RING_TRBS {
            self.dma
                .write(int_ring_off + ring_slot * trb::TRB_LEN, &zeros)?;
        }
        let base = self.phys_of(int_ring_off)?;
        let (ring, link) = ProducerRing::new(RING_TRBS, base)?;
        self.dma.write(
            int_ring_off + ring.link_slot() * trb::TRB_LEN,
            &link.to_bytes(),
        )?;
        {
            let device = self.device_mut(index).ok_or(DriverError::DeviceFault)?;
            device.int_ring = Some(ring);
        }
        self.command(Trb::new(
            TrbType::SetTrDequeuePointer,
            base | 1,
            0,
            trb::control_slot(device_slot) | trb::control_endpoint(dci),
        ))?;
        // Clear the device-side halt on the device's own interrupt-IN endpoint,
        // resetting its data toggle to match the rebuilt ring.
        let ep_addr = (dci / 2) | ENDPOINT_ADDR_DIR_IN;
        self.device_control(index, setup_clear_endpoint_halt(ep_addr), 0)?;
        Ok(())
    }
}

/// Bulk transfer serving: several TDs may be queued per direction (each
/// ring data slot pairs with its own staging buffer), completions are
/// decoded asynchronously off the shared event ring, and a device STALL is
/// recovered in place (Reset Endpoint → Set TR Dequeue Pointer →
/// `CLEAR_FEATURE(ENDPOINT_HALT)`) with every abandoned TD answered.
impl<H: XhciHost, M: DmaBank> UsbDevice<'_, H, M> {
    /// Region ring offset, staging-buffer offset, endpoint DCI, and xHCI
    /// slot of device `index`'s configured bulk endpoint for `direction`.
    ///
    /// # Errors
    ///
    /// [`DriverError::NotFound`] when no device is live at `index` or its
    /// interface carries no configured bulk endpoint in that direction.
    fn bulk_params(
        &self,
        index: usize,
        pipe: BulkPipe,
    ) -> Result<(usize, usize, u8, u8), DriverError> {
        let device = self.device(index).ok_or(DriverError::NotFound)?;
        let ring_off = device.region.bulk_ring_off(pipe);
        let bufs_off = device.region.bulk_bufs_off(pipe);
        let dci = device.bulk_dci(pipe);
        if dci == 0 {
            return Err(DriverError::NotFound);
        }
        Ok((ring_off, bufs_off, dci, device.slot))
    }

    /// TDs in flight on device `index`'s bulk ring for `pipe` (`0`
    /// when unconfigured or no device is live there).
    pub(crate) fn bulk_in_flight(&self, index: usize, pipe: BulkPipe) -> usize {
        self.device(index).map_or(0, |device| {
            device.bulk_ring(pipe).map_or(0, ProducerRing::in_flight)
        })
    }

    /// Queue one bulk-IN TD reading up to `len` bytes from device `index`,
    /// returning the ring data slot it occupies (the ticket its
    /// [`BulkComplete`] echoes). Several TDs may be queued; they complete
    /// in order through [`Self::poll_bulk`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::NotFound`] — no configured bulk-IN endpoint.
    /// * [`DriverError::LengthOutOfRange`] — `len` exceeds
    ///   [`BULK_BUF_LEN`] (the caller chunks larger transfers).
    /// * [`DriverError::Busy`] — the ring is full; poll completions first.
    pub(crate) fn queue_bulk_in(
        &mut self,
        index: usize,
        pipe: BulkPipe,
        len: usize,
    ) -> Result<usize, DriverError> {
        if pipe.direction != BulkDirection::In {
            return Err(DriverError::OutOfRange);
        }
        self.queue_bulk(index, pipe, len, None)
    }

    /// Queue one bulk-OUT TD writing `data` to device `index`, returning
    /// its ring data slot. As [`Self::queue_bulk_in`] otherwise.
    ///
    /// # Errors
    ///
    /// As [`Self::queue_bulk_in`].
    pub(crate) fn queue_bulk_out(
        &mut self,
        index: usize,
        pipe: BulkPipe,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        if pipe.direction != BulkDirection::Out {
            return Err(DriverError::OutOfRange);
        }
        self.queue_bulk(index, pipe, data.len(), Some(data))
    }

    /// Shared body of the bulk queue paths: stage the OUT bytes (when
    /// given), push one Normal TRB pointing at the slot's staging buffer,
    /// publish it, record the requested length, and ring the endpoint's
    /// doorbell.
    fn queue_bulk(
        &mut self,
        index: usize,
        pipe: BulkPipe,
        len: usize,
        data: Option<&[u8]>,
    ) -> Result<usize, DriverError> {
        let (ring_off, bufs_off, dci, device_slot) = self.bulk_params(index, pipe)?;
        if len > BULK_BUF_LEN {
            return Err(DriverError::LengthOutOfRange);
        }
        let len_u32 = u32::try_from(len).map_err(|_| DriverError::LengthOutOfRange)?;
        let slot = {
            let device = self.device(index).ok_or(DriverError::NotFound)?;
            device
                .bulk_ring(pipe)
                .ok_or(DriverError::NotFound)?
                .enqueue_slot()
        };
        // Refuse a full ring before staging, so a rejected queue leaves no
        // half-written buffer.
        if self.bulk_in_flight(index, pipe) >= BULK_SLOTS - 1 {
            return Err(DriverError::Busy);
        }
        if let Some(bytes) = data {
            self.dma.write(bufs_off + slot * BULK_BUF_LEN, bytes)?;
        }
        let buffer = self.phys_of(bufs_off + slot * BULK_BUF_LEN)?;
        let normal = Trb::new(
            TrbType::Normal,
            buffer,
            len_u32,
            trb::CONTROL_IOC | trb::CONTROL_ISP,
        );
        let (outcome, link_slot) = {
            let device = self.device_mut(index).ok_or(DriverError::NotFound)?;
            let ring = device.bulk_ring_mut(pipe).ok_or(DriverError::NotFound)?;
            (ring.push(normal)?, ring.link_slot())
        };
        publish(&mut self.dma, ring_off, link_slot, &outcome)?;
        {
            let device = self.device_mut(index).ok_or(DriverError::NotFound)?;
            device.set_bulk_len(pipe, slot, len_u32);
        }
        self.xhci.ring_doorbell(device_slot, u32::from(dci))?;
        Ok(slot)
    }

    /// Reap the next completed bulk TD, if any: halt-dropped TDs first
    /// (answered as stalled), then completions parked while a synchronous
    /// wait ran, then fresh controller events. A completed bulk-IN TD's
    /// bytes are copied into `in_buf`. Never blocks: `Ok(None)` when
    /// nothing has completed yet.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] for a controller protocol violation (a
    /// completion for a TRB never queued, out of order, or an unmodelled
    /// event type); errors of the halt recovery a stalled completion
    /// triggers. A per-TD transfer failure is **not** an error here — it is
    /// reported in the returned [`BulkComplete::result`].
    pub(crate) fn poll_bulk(
        &mut self,
        index: usize,
        in_buf: &mut [u8],
    ) -> Result<Option<BulkComplete>, DriverError> {
        let (aborted, pending) = {
            let device = self.device_mut(index).ok_or(DriverError::NotFound)?;
            let aborted = device.aborted_bulk.pop();
            let pending = if aborted.is_none() {
                device.pending_bulk.pop()
            } else {
                None
            };
            (aborted, pending)
        };
        if let Some((pipe, slot)) = aborted {
            return Ok(Some(BulkComplete {
                pipe,
                slot,
                result: Err(DriverError::EndpointStalled),
            }));
        }
        if let Some(event) = pending {
            return self.decode_bulk_event(index, event, in_buf).map(Some);
        }
        // Bounded by the event segment: one pass can hold at most the
        // segment's TRBs, and `poll_bulk` never blocks.
        for _ in 0..RING_TRBS {
            let Some(event) = self.poll_event()? else {
                return Ok(None);
            };
            match event.trb_type() {
                Ok(TrbType::PortStatusChange) => continue,
                Ok(TrbType::TransferEvent) => {}
                _ => return Err(DriverError::DeviceFault),
            }
            if self.bulk_async_index(event) == Some(index) {
                return self.decode_bulk_event(index, event, in_buf).map(Some);
            }
            // Another consumer's completion sharing the event ring (a
            // report, another device's bulk TD, the hub status-change) is
            // parked for its consumer; a stale freed-slot event is drained.
            // Anything else is a controller fault.
            if self.stash_async_event(event)? {
                continue;
            }
            return Err(DriverError::DeviceFault);
        }
        Ok(None)
    }

    /// Decode one bulk [`TrbType::TransferEvent`] (already confirmed to
    /// target a configured bulk endpoint) into its [`BulkComplete`],
    /// retiring the TD — on **every** outcome, so the software dequeue
    /// always matches the controller — and running the halt recovery on a
    /// STALL.
    fn decode_bulk_event(
        &mut self,
        index: usize,
        event: Trb,
        in_buf: &mut [u8],
    ) -> Result<BulkComplete, DriverError> {
        let pipe = self
            .device(index)
            .ok_or(DriverError::NotFound)?
            .bulk_pipe_of_dci(event.endpoint_id())
            .ok_or(DriverError::DeviceFault)?;
        let (ring_off, bufs_off, _dci, _device_slot) = self.bulk_params(index, pipe)?;
        // Map the completed TRB back to its ring slot, validating every
        // step of the controller's claim: alignment, range, and in-order
        // completion (the event must name the oldest in-flight TD).
        let ring_base = self.phys_of(ring_off)?;
        let offset = event
            .parameter
            .checked_sub(ring_base)
            .ok_or(DriverError::DeviceFault)?;
        let trb_len = trb::TRB_LEN as u64;
        if offset % trb_len != 0 {
            return Err(DriverError::DeviceFault);
        }
        let slot = usize::try_from(offset / trb_len).map_err(|_| DriverError::DeviceFault)?;
        if slot >= BULK_SLOTS {
            return Err(DriverError::DeviceFault);
        }
        {
            let device = self.device_mut(index).ok_or(DriverError::DeviceFault)?;
            let ring = device.bulk_ring_mut(pipe).ok_or(DriverError::DeviceFault)?;
            if ring.in_flight() == 0 || ring.dequeue_slot() != slot {
                return Err(DriverError::DeviceFault);
            }
            ring.retire_one()?;
        }
        match event.completion_code() {
            Ok(CompletionCode::Success | CompletionCode::ShortPacket) => {
                let requested = {
                    let device = self.device(index).ok_or(DriverError::DeviceFault)?;
                    device.bulk_len(pipe, slot)
                };
                let transferred = requested
                    .checked_sub(event.transfer_residual())
                    .ok_or(DriverError::DeviceFault)?;
                if pipe.direction == BulkDirection::In {
                    let count =
                        usize::try_from(transferred).map_err(|_| DriverError::DeviceFault)?;
                    if count > in_buf.len() {
                        return Err(DriverError::DeviceFault);
                    }
                    self.dma
                        .read(bufs_off + slot * BULK_BUF_LEN, &mut in_buf[..count])?;
                }
                Ok(BulkComplete {
                    pipe,
                    slot,
                    result: Ok(transferred),
                })
            }
            Ok(CompletionCode::StallError) => {
                // Recover the endpoint now (the abandoned TDs are answered
                // through `aborted_bulk`), then report this TD stalled; by
                // the time the caller sees it the endpoint accepts fresh
                // transfers again.
                self.recover_bulk_endpoint(index, pipe)?;
                Ok(BulkComplete {
                    pipe,
                    slot,
                    result: Err(DriverError::EndpointStalled),
                })
            }
            // A hard per-TD fault (transaction error, babble, an unmodelled
            // code): the TD is retired so the ring stays consistent, and the
            // failure is reported on this TD; the caller decides whether the
            // device is gone.
            _ => Ok(BulkComplete {
                pipe,
                slot,
                result: Err(DriverError::DeviceFault),
            }),
        }
    }

    /// Recover device `index`'s halted bulk endpoint for `direction` after
    /// a STALL (xHCI §4.8.3): answer every TD the halt abandoned, Reset
    /// Endpoint (§4.6.8), rebuild the transfer ring at its base and repoint
    /// the controller's dequeue there (§4.6.10), then clear the device-side
    /// halt so its data toggle resets (USB 2.0 §9.4.5). The endpoint
    /// accepts fresh transfers when this returns.
    fn recover_bulk_endpoint(&mut self, index: usize, pipe: BulkPipe) -> Result<(), DriverError> {
        let (ring_off, _bufs_off, dci, device_slot) = self.bulk_params(index, pipe)?;
        // Every TD still in flight was abandoned by the halt (the endpoint
        // stopped executing); answer each as stalled so no queued transfer
        // is silently lost.
        {
            let device = self.device_mut(index).ok_or(DriverError::DeviceFault)?;
            loop {
                let slot = {
                    let ring = device.bulk_ring_mut(pipe).ok_or(DriverError::DeviceFault)?;
                    if ring.in_flight() == 0 {
                        break;
                    }
                    let slot = ring.dequeue_slot();
                    ring.retire_one()?;
                    slot
                };
                device.aborted_bulk.push((pipe, slot))?;
            }
        }
        // Reset Endpoint clears the controller-side halt state.
        self.command(Trb::new(
            TrbType::ResetEndpoint,
            0,
            0,
            trb::control_slot(device_slot) | trb::control_endpoint(dci),
        ))?;
        // Rebuild the software ring at its base and drop the abandoned
        // TRBs, then point the controller's dequeue at the fresh base with
        // Dequeue Cycle State 1 to match.
        let zeros = [0u8; trb::TRB_LEN];
        for slot_index in 0..BULK_RING_TRBS {
            self.dma
                .write(ring_off + slot_index * trb::TRB_LEN, &zeros)?;
        }
        let base = self.phys_of(ring_off)?;
        let (ring, link) = ProducerRing::new(BULK_RING_TRBS, base)?;
        self.dma
            .write(ring_off + ring.link_slot() * trb::TRB_LEN, &link.to_bytes())?;
        {
            let device = self.device_mut(index).ok_or(DriverError::DeviceFault)?;
            device.set_bulk_ring(pipe, ring);
        }
        self.command(Trb::new(
            TrbType::SetTrDequeuePointer,
            base | 1,
            0,
            trb::control_slot(device_slot) | trb::control_endpoint(dci),
        ))?;
        // Clear the device-side halt on the device's own control endpoint
        // (never the hub's), resetting its data toggle.
        let ep_addr = match pipe.direction {
            BulkDirection::In => (dci / 2) | ENDPOINT_ADDR_DIR_IN,
            BulkDirection::Out => dci / 2,
        };
        self.device_control(index, setup_clear_endpoint_halt(ep_addr), 0)?;
        Ok(())
    }
}

impl<'w, H: XhciHost, M: DmaBank> UsbDevice<'w, H, M> {
    /// Deliver device `index`'s next interrupt-IN report into `buf` from the
    /// engine's report buffer, keeping the endpoint armed to depth and
    /// draining any freshly-posted completions first. Never blocks: `Ok(None)`
    /// when no report is buffered (the class driver's URB stays parked).
    ///
    /// The report was captured off the controller's ring — here or, under
    /// load, already by the HCD's per-interrupt [`Self::pump_reports`] — so a
    /// starved class driver collects buffered reports rather than losing them.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] when no device is live at `index` or the
    /// device's interface carries no interrupt endpoint; or a recorded fatal
    /// report completion (a device-gone / fail-closed code) surfaced once so
    /// the HCD confirms the port and detaches; or a fault reading the event
    /// ring.
    pub fn next_report(
        &mut self,
        index: usize,
        buf: &mut [u8],
    ) -> Result<Option<usize>, DriverError> {
        {
            let Some(device) = self.device(index) else {
                // Not enumerated: there is no endpoint to drain.
                return Err(DriverError::DeviceFault);
            };
            if device.int_dci == DCI_CONTROL || device.int_ring.is_none() {
                // A bulk-only interface has no interrupt endpoint to read.
                return Err(DriverError::DeviceFault);
            }
        }
        // Keep the endpoint armed to depth and capture whatever the
        // controller has posted into the report FIFO. Capture is driven off
        // the controller interrupt too (`pump_reports`), so under load a
        // report is usually already buffered here; draining again is cheap
        // and keeps the class-driver path self-contained.
        self.ensure_reports_armed(index)?;
        self.drain_events()?;
        // Recover the interrupt endpoint if a halting completion flagged it.
        // This is the top-level, non-re-entrant recovery point: `drain_events`
        // above only *flags* a halt (it may run re-entrantly), so the actual
        // reset/rebuild/CLEAR_FEATURE happens here where it cannot recurse.
        self.recover_report_endpoint_if_pending(index)?;
        // Deliver the oldest buffered report, if any.
        if let Some(report) = self
            .device_mut(index)
            .and_then(|device| device.reports.pop())
        {
            let len = usize::from(report.len).min(buf.len());
            buf[..len].copy_from_slice(&report.data[..len]);
            return Ok(Some(len));
        }
        // No report buffered. Surface a recorded fatal report fault (a
        // device-gone or fail-closed completion) once, so the HCD confirms
        // the port and detaches; otherwise the URB stays parked (`Ok(None)`).
        if let Some(err) = self
            .device_mut(index)
            .and_then(|device| device.report_fault.take())
        {
            return Err(err);
        }
        Ok(None)
    }

    /// Capture every completed transfer the controller has posted on the
    /// shared event ring, so every served interrupt-IN endpoint's reports land
    /// in its device's FIFO and its ring is re-armed to depth — **without**
    /// needing a class-driver URB to be outstanding.
    ///
    /// This is the consumer-independent report path: called on every
    /// controller interrupt (by the HCD's [`Self::pump_reports`]) and again
    /// from each [`Self::next_report`], it decouples device polling from how
    /// promptly any class driver — or the HCD itself — is scheduled to run.
    /// A boot keyboard, a mouse, a CBI completion endpoint, or any other
    /// interrupt-IN device is therefore captured the moment its report lands
    /// in the controller's ring, so a report is never dropped merely because
    /// the software above was starved of CPU (the on-metal "missed keypresses
    /// under load" defect, generalised to every interrupt-IN device).
    ///
    /// # Errors
    ///
    /// [`DriverError`] from reading the event ring (a register-window fault).
    /// Per-event capture faults are recorded on the owning device rather than
    /// propagated (see [`Self::capture_report_event`]); an unattributable or
    /// informational event is drained and ignored (the shared ring is not a
    /// security boundary, and a stray event must never fault the report path).
    fn drain_events(&mut self) -> Result<(), DriverError> {
        for _ in 0..EVENT_DRAIN_BOUND {
            let Some(event) = self.poll_event()? else {
                return Ok(());
            };
            if event.trb_type() == Ok(TrbType::TransferEvent) {
                // Report completions are captured into their device's FIFO,
                // hub/bulk completions parked, freed-slot events tolerated.
                // A capture-side controller/DMA hiccup is not fatal to the
                // shared drain: the endpoint is re-armed on the next pass, and
                // any real fatal report fault is surfaced through
                // `next_report` from the recorded per-device flag.
                let _ = self.stash_async_event(event);
            }
            // Everything else (a Port Status Change, an unmodelled or stale
            // event) is drained by the `poll_event` above and ignored.
        }
        Ok(())
    }

    /// Keep every served interrupt-IN endpoint armed and drain all pending
    /// report completions into the per-device FIFOs — the HCD's per-interrupt
    /// report pump.
    ///
    /// The host-controller driver calls this on every controller completion
    /// interrupt, **before** it services any outstanding class-driver URB, so
    /// the controller always has landing TRBs and every report is captured the
    /// moment it arrives — regardless of whether the class driver has a URB in
    /// flight, and regardless of how starved that class driver is. A class
    /// driver then merely collects the already-buffered reports when it next
    /// submits ([`Self::next_report`]); its consume rate no longer gates
    /// hardware polling. This is what makes the report path immune to a
    /// CPU-starved consumer, for every interrupt-IN device the controller
    /// serves.
    ///
    /// # Errors
    ///
    /// [`DriverError`] from reading the event ring. A per-device arming hiccup
    /// (e.g. a device that has just vanished) is tolerated: the drain and the
    /// hub/root hot-plug watch handle its teardown.
    pub fn pump_reports(&mut self) -> Result<(), DriverError> {
        for index in 0..self.devices.len() {
            if self
                .device(index)
                .is_some_and(|device| device.int_dci != DCI_CONTROL && device.int_ring.is_some())
            {
                let _ = self.ensure_reports_armed(index);
            }
        }
        self.drain_events()?;
        // Recover any interrupt endpoint a halting completion flagged during
        // the drain above. `drain_events` only *flags* a halt (it may run
        // re-entrantly from a synchronous wait); the reset/rebuild/CLEAR_FEATURE
        // happens here, at this top-level, non-re-entrant point. A per-device
        // recovery hiccup (a device that vanished mid-recovery) is tolerated —
        // its teardown is driven by the hub/root watch and the recorded report
        // fault — so it never aborts the whole pump.
        for index in 0..self.devices.len() {
            let _ = self.recover_report_endpoint_if_pending(index);
        }
        Ok(())
    }

    /// Total interrupt-IN reports dropped across all served devices because a
    /// class driver fell more than [`REPORT_QUEUE_CAP`] reports behind.
    ///
    /// A genuinely stalled consumer (one that has stopped reading for many
    /// report intervals) cannot make the engine hold unbounded memory: the
    /// oldest buffered report is dropped so the newest device state is kept.
    /// This count lets the HCD surface that loss for diagnostics rather than
    /// hiding it.
    #[must_use]
    pub fn dropped_report_total(&self) -> u64 {
        self.devices
            .iter()
            .flatten()
            .map(|device| device.dropped_reports)
            .sum()
    }

    /// Decode one completed interrupt-IN transfer `event` for device `index`,
    /// retire the ring slot, and buffer the result — the capture half of the
    /// report path (the delivery half is [`Self::next_report`]).
    ///
    /// The transfer is retired **unconditionally** first (so an unexpected
    /// completion code or malformed buffer mapping still advances the ring —
    /// a single odd transfer must never silence the device). Then:
    ///
    /// * a real report is copied out of its DMA slot and **enqueued** into the
    ///   device's report FIFO ([`Self::enqueue_report`]), and the endpoint is
    ///   re-armed to depth — so its bytes survive the slot being reused;
    /// * a *successful* zero-length completion (a ZLP) carries no report: the
    ///   endpoint is simply re-armed (an idle or ZLP-streaming HID collection
    ///   neither buffers empty reports nor is faulted);
    /// * a *successful* completion whose claimed buffer mapping is malformed
    ///   (a forged residual or TRB pointer) leaves the endpoint un-halted, so
    ///   there is nothing to recover: it **records** a fatal report fault on
    ///   the device for [`Self::next_report`] to surface once;
    /// * any halting code — a STALL, babble, data-buffer or TRB error, and a
    ///   USB or split *transaction* error alike — leaves the endpoint halted
    ///   (the controller runs no further transfers on it until it is reset).
    ///   It is only **flagged** here ([`DeviceState::int_recovery_pending`]);
    ///   the endpoint is recovered later, at a top-level, non-re-entrant point
    ///   ([`Self::recover_report_endpoint_if_pending`]). Recovery cannot run
    ///   here because this capture also runs re-entrantly from inside a
    ///   synchronous command/EP0 wait, and recovery itself waits — so
    ///   recovering here would recurse into recovery and scramble the ring it
    ///   is rebuilding (the on-metal defect where input hammered during
    ///   bring-up killed the class driver). A halting code is not, on its own,
    ///   a hot-removal: a transaction error is exactly what a present-but-
    ///   disturbed device produces, and the deferred recovery is the
    ///   authoritative liveness test.
    ///
    /// A fault is never propagated to the caller: capture runs from the shared
    /// drain and from synchronous EP0/command waits, where an asynchronous
    /// report must not fault unrelated work. Recorded faults surface only
    /// through [`Self::next_report`]; a halted endpoint is recovered by
    /// [`Self::recover_report_endpoint_if_pending`].
    fn capture_report_event(&mut self, index: usize, event: Trb) -> Result<(), DriverError> {
        // A recovery for this endpoint is in progress (this capture is running
        // re-entrantly from inside one of recovery's own synchronous
        // command/CLEAR_FEATURE waits). Recovery is rebuilding the ring, so
        // touching it here would corrupt it and re-entering recovery would
        // recurse without bound — the on-metal defect where input hammered
        // during bring-up killed the class driver. Leave the ring untouched
        // and keep the endpoint flagged so recovery runs again after this one
        // completes (the endpoint re-halted on this fresh completion).
        if self
            .device(index)
            .is_some_and(|device| device.int_recovering)
        {
            if let Some(device) = self.device_mut(index) {
                device.int_recovery_pending = true;
            }
            return Ok(());
        }
        let mut data = [0u8; CAPTURE_LEN];
        let decoded = self.decode_transfer_report(index, event, &mut data);
        self.retire_interrupt_transfer(index)?;
        match decoded {
            Ok(Some(len)) => {
                self.enqueue_captured_report(index, &data[..len]);
                self.ensure_reports_armed(index)?;
            }
            Ok(None) => {
                self.ensure_reports_armed(index)?;
            }
            Err(err) => match event.completion_code() {
                // A *successful* completion whose claimed mapping the decode
                // rejected (a hostile/buggy residual or TRB pointer). The
                // endpoint is not halted, so there is nothing to recover:
                // record the fault for the class driver's next URB to fail
                // closed on.
                Ok(CompletionCode::Success | CompletionCode::ShortPacket) => {
                    self.record_report_fault(index, err);
                }
                // Any halting completion — a STALL, babble, data-buffer or TRB
                // error, and a USB or split *transaction* error alike — leaves
                // the endpoint halted (the controller runs no further transfers
                // on it until it is reset). A halting code is NOT, on its own,
                // conclusive of a hot-removal: a transaction error (CRC,
                // timeout, bad PID) is exactly what a device that is present
                // but momentarily disturbed produces — for instance one
                // hammered with input while its interrupt endpoint is still
                // being brought up.
                //
                // Recovery MUST NOT run here. This capture happens from the
                // shared drain and, crucially, re-entrantly from inside a
                // synchronous EP0/command wait: recovery issues Reset Endpoint
                // / Set TR Dequeue commands and a device-side CLEAR_FEATURE,
                // each of which itself waits on the shared event ring, during
                // which another interrupt completion for this same endpoint can
                // arrive and land back here — recursing into recovery and
                // scrambling the ring it is rebuilding. So only *flag* the
                // endpoint; the real, non-re-entrant recovery is performed at
                // top level by `recover_report_endpoint_if_pending` (from
                // `next_report` and `pump_reports`). The rejected completion
                // code stays captured in `last_report_fault_code` (set by
                // `decode_transfer_report`) for the detach path.
                _ => {
                    if let Some(device) = self.device_mut(index) {
                        device.int_recovery_pending = true;
                    }
                }
            },
        }
        Ok(())
    }

    /// Recover device `index`'s interrupt-IN endpoint if a halting completion
    /// flagged it ([`DeviceState::int_recovery_pending`]) — the top-level,
    /// **non-re-entrant** half of the halt-recovery split.
    ///
    /// Called only from [`Self::next_report`] and [`Self::pump_reports`],
    /// which run from the HCD's URB service and its per-interrupt report pump
    /// — never from inside a synchronous command/EP0 wait. A guard
    /// ([`DeviceState::int_recovering`]) makes any interrupt completion that
    /// arrives during recovery's own waits re-flag the endpoint instead of
    /// recursing, so recovery can never re-enter itself.
    ///
    /// Recovery is the authoritative liveness test, attempted once per pending
    /// halt:
    ///
    /// * it succeeds — the device is present and answered its own
    ///   `CLEAR_FEATURE` handshake — so the endpoint is re-armed, the pending
    ///   flag and the captured fault code are cleared, and the class driver's
    ///   URB stays parked (a transient fault never reaches it);
    /// * it faults — a genuinely vanished device cannot complete the
    ///   handshake — so the fatal report fault is recorded for
    ///   [`Self::next_report`] to surface once (the HCD then confirms the port
    ///   and detaches), with `last_report_fault_code` still holding the
    ///   device-unreachable code so a gone device behind a still-connected hub
    ///   port is freed directly.
    ///
    /// # Errors
    ///
    /// Never propagates a recovery fault (a vanished device is a recorded
    /// report fault, not an error here); only a bookkeeping fault re-arming a
    /// recovered endpoint is surfaced.
    fn recover_report_endpoint_if_pending(&mut self, index: usize) -> Result<(), DriverError> {
        if !self
            .device(index)
            .is_some_and(|device| device.int_recovery_pending)
        {
            return Ok(());
        }
        // Clear the pending flag before recovering: a fresh halt observed
        // *during* recovery (via the `int_recovering` guard) sets it again, so
        // a re-halt is not lost, while a clean recovery leaves it clear.
        if let Some(device) = self.device_mut(index) {
            device.int_recovery_pending = false;
            device.int_recovering = true;
        }
        let outcome = self.recover_interrupt_endpoint(index);
        if let Some(device) = self.device_mut(index) {
            device.int_recovering = false;
        }
        match outcome {
            Ok(()) => {
                // Present: no fault to surface. Clear the captured fault code
                // (a transient transaction error must not linger and be read
                // as a removal by a later detach check) and re-arm to depth.
                if let Some(device) = self.device_mut(index) {
                    device.last_report_fault_code = 0;
                }
                self.ensure_reports_armed(index)
            }
            // Gone: the device could not complete its own recovery handshake.
            // Record the fault so the URB path confirms and detaches; the
            // hub/root watch also detaches independently, so a starved consumer
            // still loses the device cleanly. `DeviceFault` is the stable
            // client-visible errno; `last_report_fault_code` (the rejected
            // completion code) is what drives the detach.
            Err(_recover_err) => {
                self.record_report_fault(index, DriverError::DeviceFault);
                Ok(())
            }
        }
    }

    /// Buffer a captured interrupt-IN report for device `index`, read the way
    /// this interface's [`ReportDecode`] says.
    ///
    /// A report that does not match this interface's map — a different Report
    /// ID (a sibling collection sharing the endpoint) or one truncated below
    /// the fields the map names — is dropped, exactly like an idle no-op
    /// report: it is not this interface's mouse/keyboard report, so nothing is
    /// buffered and the class driver's URB stays parked. An interface on the
    /// boot-protocol fallback buffers the report as delivered, and a refused
    /// one buffers nothing at all.
    fn enqueue_captured_report(&mut self, index: usize, raw: &[u8]) {
        let decode = self
            .device(index)
            .map_or(ReportDecode::Boot, |d| d.report_decode);
        let map = match decode {
            // Boot-protocol fallback: the report is buffered as delivered.
            ReportDecode::Boot => {
                self.enqueue_report(index, raw);
                return;
            }
            // No safe reading of this device's reports, so none is delivered.
            ReportDecode::Refused => return,
            ReportDecode::Mapped(map) => map,
        };
        // Report-protocol normalisation may drop a report (a report-ID
        // mismatch — a sibling collection sharing the endpoint — or a field
        // past the captured bytes), exactly like an idle no-op: nothing is
        // buffered and the class driver's URB stays parked.
        let mut boot = [0u8; REPORT_LEN];
        if let Some(n) = map.normalize(raw, &mut boot) {
            self.enqueue_report(index, &boot[..n]);
        }
    }

    /// Buffer one decoded report for device `index`, dropping the **oldest**
    /// buffered report (and counting the loss) when the FIFO is full.
    ///
    /// The FIFO is bounded ([`REPORT_QUEUE_CAP`]): a class driver that has
    /// stopped reading for many report intervals cannot make the engine hold
    /// unbounded memory. Dropping the oldest keeps the FIFO in order and
    /// leaves the newest device state for a consumer that catches up.
    fn enqueue_report(&mut self, index: usize, bytes: &[u8]) {
        let Some(device) = self.device_mut(index) else {
            return;
        };
        let n = bytes.len().min(REPORT_LEN);
        let mut report = Report {
            data: [0u8; REPORT_LEN],
            #[allow(clippy::cast_possible_truncation)] // `n <= REPORT_LEN` (a small constant).
            len: n as u8,
        };
        report.data[..n].copy_from_slice(&bytes[..n]);
        if device.reports.push(report).is_err() {
            // The consumer is not draining: drop the oldest to keep the
            // newest, and count the loss so it is never silent.
            let _ = device.reports.pop();
            device.dropped_reports = device.dropped_reports.saturating_add(1);
            let _ = device.reports.push(report);
        }
    }

    /// Record a fatal report completion on device `index` for the next
    /// [`Self::next_report`] to surface, keeping the first observed fault.
    fn record_report_fault(&mut self, index: usize, err: DriverError) {
        if let Some(device) = self.device_mut(index) {
            device.report_fault.get_or_insert(err);
        }
    }

    /// A borrowed engine view serving one device's URB transfers: the
    /// [`UrbEngine`](crate::transport::UrbEngine) the HCD's per-interface
    /// URB service drives for the device at `index`.
    pub fn engine_for(&mut self, index: usize) -> DeviceEngine<'_, 'w, H, M> {
        DeviceEngine {
            engine: self,
            index,
        }
    }
}

/// One served device's [`UrbEngine`](crate::transport::UrbEngine) view over
/// the shared controller engine ([`UsbDevice::engine_for`]).
///
/// Every transfer it drives targets exactly the device at its index —
/// control transfers activate that device's EP0 ring, interrupt reads drain
/// its report endpoint, bulk transfers use its ring pair — so one
/// interface's URB service can never reach another device's endpoints.
pub struct DeviceEngine<'a, 'w, H: XhciHost, M: DmaBank> {
    engine: &'a mut UsbDevice<'w, H, M>,
    index: usize,
}

impl<H: XhciHost, M: DmaBank> tairix_abi::driver::input::ReportSource
    for DeviceEngine<'_, '_, H, M>
{
    fn next_report(&mut self, buf: &mut [u8]) -> Result<Option<usize>, DriverError> {
        self.engine.next_report(self.index, buf)
    }
}

impl<H: XhciHost, M: DmaBank> crate::transport::UrbEngine for DeviceEngine<'_, '_, H, M> {
    fn control_in(&mut self, setup: [u8; 8], data: &mut [u8]) -> Result<usize, DriverError> {
        // The engine's control transfer lands the IN data in the shared
        // control-data DMA buffer; copy out only the bytes the device
        // delivered, never past the caller's shared buffer. It targets this
        // *device* — for a hub-downstream device the device's EP0 ring is
        // activated for the transfer, never the hub's.
        let requested = u32::try_from(data.len()).map_err(|_| DriverError::LengthOutOfRange)?;
        let transferred = self.engine.device_control(self.index, setup, requested)?;
        let transferred = usize::try_from(transferred).map_err(|_| DriverError::DeviceFault)?;
        let copied = transferred.min(data.len());
        self.engine
            .dma
            .read(self.engine.layout.ctrl_data, &mut data[..copied])?;
        Ok(copied)
    }

    fn control_no_data(&mut self, setup: [u8; 8]) -> Result<(), DriverError> {
        // No data stage: the request's whole meaning rides in SETUP (the
        // engine's control path builds a SETUP + status-IN transfer when the
        // data length is zero). It targets this *device* exactly as
        // `control_in` does — never the hub above it.
        self.engine.device_control(self.index, setup, 0).map(|_| ())
    }

    fn control_out(&mut self, setup: [u8; 8], data: &[u8]) -> Result<(), DriverError> {
        // An OUT data stage carrying the shared buffer's bytes (the CBI
        // ADSC command channel). It targets this *device* exactly as
        // `control_in` does — never the hub above it.
        self.engine.device_control_out(self.index, setup, data)
    }

    fn interrupt_in(&mut self, data: &mut [u8]) -> Result<Option<usize>, DriverError> {
        self.engine.next_report(self.index, data)
    }

    fn bulk_in(&mut self, endpoint: u8, data: &mut [u8]) -> Result<Option<usize>, DriverError> {
        // The URB names an endpoint *number*; it must be one of the
        // interface's configured bulk-IN endpoints (an IN DCI is `2n + 1`).
        let pipe = self
            .bulk_pipe_for(endpoint, BulkDirection::In)
            .ok_or(DriverError::OutOfRange)?;
        // The URB transport holds one URB outstanding per interface: arm
        // the TD on first drive, reap its completion on a later one.
        if self.engine.bulk_in_flight(self.index, pipe) == 0 {
            self.engine.queue_bulk_in(self.index, pipe, data.len())?;
            return Ok(None);
        }
        match self.engine.poll_bulk(self.index, data)? {
            Some(complete) if complete.pipe == pipe => match complete.result {
                Ok(n) => Ok(Some(
                    usize::try_from(n).map_err(|_| DriverError::DeviceFault)?,
                )),
                Err(err) => Err(err),
            },
            // A completion for another pipe cannot belong to the one
            // outstanding URB — a protocol violation, surfaced.
            Some(_) => Err(DriverError::DeviceFault),
            None => Ok(None),
        }
    }

    fn bulk_out(&mut self, endpoint: u8, data: &[u8]) -> Result<Option<usize>, DriverError> {
        let pipe = self
            .bulk_pipe_for(endpoint, BulkDirection::Out)
            .ok_or(DriverError::OutOfRange)?;
        if self.engine.bulk_in_flight(self.index, pipe) == 0 {
            self.engine.queue_bulk_out(self.index, pipe, data)?;
            return Ok(None);
        }
        // An OUT completion carries no device bytes to copy back.
        let mut no_in_bytes = [0u8; 0];
        match self.engine.poll_bulk(self.index, &mut no_in_bytes)? {
            Some(complete) if complete.pipe == pipe => match complete.result {
                Ok(n) => Ok(Some(
                    usize::try_from(n).map_err(|_| DriverError::DeviceFault)?,
                )),
                Err(err) => Err(err),
            },
            Some(_) => Err(DriverError::DeviceFault),
            None => Ok(None),
        }
    }
}

impl<H: XhciHost, M: DmaBank> DeviceEngine<'_, '_, H, M> {
    /// The configured bulk pipe of this device whose endpoint *number* is
    /// `endpoint` in `direction`, `None` when no configured pipe matches
    /// (the URB is refused fail-closed).
    fn bulk_pipe_for(&self, endpoint: u8, direction: BulkDirection) -> Option<BulkPipe> {
        let device = self.engine.device(self.index)?;
        let dci = match direction {
            BulkDirection::In => endpoint * 2 + 1,
            BulkDirection::Out => endpoint * 2,
        };
        let pipe = device.bulk_pipe_of_dci(dci)?;
        (pipe.direction == direction).then_some(pipe)
    }
}
