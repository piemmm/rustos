//! TAIRiX virtio-input device logic (keyboard / pointer).
//!
//! The arch-neutral, transport-agnostic open/poll/decode engine for a
//! virtio-input device, implementing [`tairix_abi::driver::input::Input`]
//! on top of the cross-arch virtio transport from `lib/virtio`. As with
//! `virtio_blk` / `virtio_net`, the logic is bus-agnostic: the same
//! source drives the PCI and MMIO transports (the
//! queue protocol lives once, in the transport crate).
//!
//! It lives in `lib/*` so both the in-kernel `-M virt` input verticals
//! and the user-space input-driver process compose it without a
//! `drivers/*`→`drivers/*` dependency (the
//! virtio analogue of `lib/hid` ↔ `drivers/input/usb_kbd`). The thin
//! `drivers/input/virtio_input` crate keeps only the §8 `register` entry
//! and the bind table built from [`VIRTIO_INPUT_DEVICE_ID`].
//!
//! # Wire protocol
//!
//! Virtio 1.1. A virtio-input device exposes two virtqueues —
//! the **eventq** (index 0), on which the device delivers
//! `struct virtio_input_event` records to the driver, and the
//! **statusq** (index 1), on which the driver returns status (LEDs,
//! force-feedback) to the device. This logic consumes the eventq
//! only; the statusq is optional and left unprogrammed (`abi-v1`
//! input reports events, it does not drive device feedback).
//!
//! Each event is the 8-byte little-endian record
//! `{ __le16 type; __le16 code; __le32 value; }` (virtio 1.1 §5.8.6).
//! The `type`/`code` namespaces are the Linux `evdev` ones, so the
//! decode below maps `EV_KEY` to [`InputEventKind::Key`] and `EV_REL`
//! pointer / wheel axes to [`InputEventKind::Pointer`] /
//! [`InputEventKind::Scroll`], discarding the `EV_SYN` frame markers
//! and any namespace this `abi-v1` surface does not model.
//!
//! No feature bits are negotiated: the only virtio-input feature
//! (`VIRTIO_INPUT_F_*` selects config-space reporting, which this
//! event-path logic does not use), so it accepts the empty
//! feature subset.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod console;

pub use console::VirtioKeyboardConsole;

use tairix_abi::driver::input::{Input, InputEvent, InputEventKind, AXIS_X, AXIS_Y};
use tairix_abi::driver::BufferClass;
use tairix_abi::DriverError;
use tairix_virtio::{
    BounceBuffer, ChainSegment, Direction, SplitQueue, Status, Transport, VirtioError, VirtioHost,
};

/// The virtio device id of an input device (virtio 1.1 §5.8 —
/// `virtio-input` is device type 18). The `drivers/input/virtio_input`
/// bind table's match key is built from it, so a discovered virtio node
/// whose probed device id is 18 binds that driver and nothing else; it
/// lives here as the single source of truth the device logic and the
/// driver crate's `BIND_KEYS` both depend on.
pub const VIRTIO_INPUT_DEVICE_ID: u32 = 18;

/// Virtio-input wire-protocol constants (virtio 1.1 §5.8 + the Linux
/// `evdev` `type`/`code` namespaces the device reports in).
mod wire {
    /// Event virtqueue index (device → driver), virtio 1.1 §5.8.2.
    pub const EVENT_QUEUE: u16 = 0;
    /// Event-queue depth ceiling (descriptors), power-of-two per virtio
    /// §2.6; the programmed depth is the device's advertised
    /// `queue_max_size` clamped to this (QEMU's virtio-input advertises
    /// 64). The whole pool stays posted, and its depth is the loss
    /// bound: the device **silently drops** events when no posted buffer
    /// is free (virtio 1.1 §5.8.6.2), and the driver's drain can lag
    /// whole bursts behind a saturated CPU (a busy desktop re-rendering
    /// while a click arrives), so a shallow pool loses real input — a
    /// click's press/release vanishing mid-burst, observed end to end
    /// before this depth was raised from eight. Sixty-four single-event
    /// buffers (512 bytes of bounce memory) absorb every realistic input
    /// burst between two driver wakes.
    pub const EVENT_QUEUE_SIZE: u16 = 64;
    /// Byte length of one `struct virtio_input_event`
    /// (`__le16 type`, `__le16 code`, `__le32 value`), virtio 1.1 §5.8.6.
    /// A `u32` so it feeds a descriptor `len` directly; widen to `usize`
    /// (lint-free) for slice/allocation sizes.
    pub const EVENT_LEN: u32 = 8;

    /// `VIRTIO_F_VERSION_1` (feature bit 32): the modern virtio 1.x
    /// split-virtqueue layout. Required of a non-transitional device
    /// (virtio 1.1 §6.1); QEMU's `force-legacy=false` virtio-input only
    /// makes the driver's posted eventq buffers visible to the device
    /// once this bit is acked.
    pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

    /// `EV_SYN` — event-frame separator (Linux `evdev`). Carries no
    /// surfaced event.
    pub const EV_SYN: u16 = 0x00;
    /// `EV_KEY` — key / button press or release.
    pub const EV_KEY: u16 = 0x01;
    /// `EV_REL` — relative pointer / wheel motion.
    pub const EV_REL: u16 = 0x02;

    /// `REL_X` — relative motion along the X axis.
    pub const REL_X: u16 = 0x00;
    /// `REL_Y` — relative motion along the Y axis.
    pub const REL_Y: u16 = 0x01;
    /// `REL_WHEEL` — vertical scroll-wheel motion.
    pub const REL_WHEEL: u16 = 0x08;
}

/// Decode one raw `virtio_input_event` triple into the platform-neutral
/// [`InputEvent`].
///
/// Returns `None` for frame markers (`EV_SYN`) and any `type`/`code`
/// this `abi-v1` input surface does not model, so the caller treats a
/// consumed-but-unmapped event as "no event" rather than fabricating a
/// bogus one (fail closed, never guess).
fn decode_event(etype: u16, code: u16, value: i32) -> Option<InputEvent> {
    // `EV_SYN` is kept as its own arm: a frame separator is a distinct,
    // expected protocol case (virtio 1.1 §5.8.6) that we deliberately
    // drop, documented apart from the catch-all for unknown types even
    // though both yield `None`.
    #[allow(clippy::match_same_arms)]
    match etype {
        wire::EV_KEY => Some(InputEvent {
            kind: InputEventKind::Key,
            reserved0: 0,
            code,
            value,
        }),
        wire::EV_REL => match code {
            wire::REL_X => Some(InputEvent {
                kind: InputEventKind::Pointer,
                reserved0: 0,
                code: AXIS_X,
                value,
            }),
            wire::REL_Y => Some(InputEvent {
                kind: InputEventKind::Pointer,
                reserved0: 0,
                code: AXIS_Y,
                value,
            }),
            wire::REL_WHEEL => Some(InputEvent {
                kind: InputEventKind::Scroll,
                reserved0: 0,
                code: AXIS_Y,
                value,
            }),
            _ => None,
        },
        // Frame separator: end of an event group, no surfaced event.
        wire::EV_SYN => None,
        // Every other evdev namespace carries no surfaced event here.
        _ => None,
    }
}

/// Input device backed by a cross-arch virtio transport.
///
/// `'h` bounds the borrow of the [`VirtioHost`] the driver allocates
/// its DMA regions through; the host is minted per driver load and
/// lives only for the duration of that load, so the driver borrows it
/// for `'h` rather than demanding a `'static` host (per-process pools are reclaimed when the driver unloads). This
/// mirrors [`VirtioNet`](../tairix_drv_network_virtio_net/struct.VirtioNet.html).
pub struct VirtioInput<'h, T: Transport> {
    transport: T,
    eventq: SplitQueue,
    host: &'h dyn VirtioHost,
    /// One shared device-writable region holding every event slot back
    /// to back (`negotiated depth × EVENT_LEN` bytes — a fraction of a
    /// page, never a page per 8-byte event). The device fills one slot
    /// per `virtio_input_event` it delivers; the driver keeps every
    /// slot posted so several events (e.g. an `EV_KEY` plus its
    /// `EV_SYN` frame separator) can be in flight at once — a single
    /// posted buffer is not enough, because the device needs a free
    /// buffer for *every* event of a report, including the `EV_SYN`
    /// (virtio 1.1 §5.8.6).
    event_pool: BounceBuffer,
    /// Descriptor head → pool slot index for every in-flight slot. The
    /// queue assigns heads from its own free list, so the map is
    /// re-recorded on every repost.
    event_slots: [Option<u16>; wire::EVENT_QUEUE_SIZE as usize],
}

impl<'h, T: Transport> VirtioInput<'h, T> {
    /// Bring the device online and post the event-buffer pool.
    ///
    /// Implements the virtio-1.1 §3.1 initialisation sequence: reset,
    /// ACKNOWLEDGE, DRIVER, feature negotiation (`VIRTIO_F_VERSION_1`
    /// only — the modern split-virtqueue layout, no device-specific
    /// features), `FEATURES_OK`, set up the event queue, `DRIVER_OK`,
    /// then fill the eventq with one device-write slot per negotiated
    /// descriptor (all carved from one shared DMA region) and notify
    /// the device.
    ///
    /// # Errors
    ///
    /// Propagates the transport / queue-setup [`VirtioError`] (mapped to
    /// [`DriverError`]), [`DriverError::DeviceFault`] if the device
    /// clears [`Status::FEATURES_OK`] after negotiation, and any
    /// [`DriverError`] from the DMA-buffer allocation.
    pub fn open(mut transport: T, host: &'h dyn VirtioHost) -> Result<Self, DriverError> {
        transport.reset();
        let mut status = Status::default().with(Status::ACKNOWLEDGE);
        transport.set_status(status);
        status = status.with(Status::DRIVER);
        transport.set_status(status);
        // Negotiate `VIRTIO_F_VERSION_1` (bit 32): the modern virtio 1.x
        // split-virtqueue layout, required of a non-transitional device
        // (QEMU's `force-legacy=false`). No device-specific feature bits
        // are negotiated.
        let device_features = transport.device_features();
        let driver_features = device_features & wire::VIRTIO_F_VERSION_1;
        transport.set_driver_features(driver_features);
        status = status.with(Status::FEATURES_OK);
        transport.set_status(status);
        if !transport.status().contains(Status::FEATURES_OK) {
            return Err(VirtioError::FeaturesRejected.as_driver_error());
        }
        // Program the deepest event queue the device supports, up to the
        // pool ceiling: depth is the input-loss bound (the device drops
        // events with no posted buffer), so take everything offered. A
        // device advertising a zero-sized queue is broken; refuse it
        // rather than run a driver that can never receive an event.
        transport
            .queue_select(wire::EVENT_QUEUE)
            .map_err(VirtioError::as_driver_error)?;
        let queue_size = transport.queue_max_size().min(wire::EVENT_QUEUE_SIZE);
        if queue_size == 0 {
            return Err(DriverError::DeviceFault);
        }
        let mut eventq = SplitQueue::new(&mut transport, host, wire::EVENT_QUEUE, queue_size)
            .map_err(VirtioError::as_driver_error)?;
        status = status.with(Status::DRIVER_OK);
        transport.set_status(status);

        // One region carries every slot: the depth is bounded by the
        // 64-entry ceiling, so the whole pool is 512 bytes — never a DMA
        // page per 8-byte event.
        let region = host.alloc_dma_zeroed(usize::from(queue_size) * wire::EVENT_LEN as usize)?;
        let event_pool = BounceBuffer::new(region, BufferClass::NonSensitive);
        let mut event_slots: [Option<u16>; wire::EVENT_QUEUE_SIZE as usize] =
            core::array::from_fn(|_| None);
        for slot in 0..queue_size {
            Self::post_slot(&mut eventq, &event_pool, slot, &mut event_slots)?;
        }
        eventq.kick(&mut transport);

        Ok(Self {
            transport,
            eventq,
            host,
            event_pool,
            event_slots,
        })
    }

    /// Bring the device online ([`Self::open`]) and only then run the
    /// caller's `arm` step — the driver's externally observable
    /// readiness action, e.g. binding the granted device interrupt
    /// (the audited `irq_bind` syscall a test harness or supervisor
    /// watches for).
    ///
    /// The ordering is the point of this constructor. A virtio-input
    /// device silently discards events while its eventq has no posted
    /// buffers, so an `arm` step performed *before* [`Self::open`]
    /// advertises readiness while a keystroke can still be dropped —
    /// the lost-first-keypress race observed on the autoload input
    /// vertical. Running `arm` strictly after the eventq is live
    /// (`DRIVER_OK` set, every buffer posted, the device kicked) makes
    /// the arm step a truthful readiness witness; an event that
    /// arrives between the kick and the `arm` return sits in the used
    /// ring and is collected by [`Input::poll`]'s pre-wait drain, so
    /// nothing is lost in that window either.
    ///
    /// `arm` must not wait for input (it runs before the event pump
    /// exists); it performs its one readiness action and returns.
    ///
    /// # Errors
    ///
    /// Propagates [`Self::open`]'s errors unchanged. If `arm` fails,
    /// the device is torn down ([`Self::close`], so a live device is
    /// never left DMA-writing into a driver that is about to exit)
    /// and the `arm` error is returned.
    pub fn open_armed<F>(
        transport: T,
        host: &'h dyn VirtioHost,
        arm: F,
    ) -> Result<Self, DriverError>
    where
        F: FnOnce(&mut Self) -> Result<(), DriverError>,
    {
        let mut input = Self::open(transport, host)?;
        if let Err(e) = arm(&mut input) {
            input.close();
            return Err(e);
        }
        Ok(input)
    }

    /// Tear the device down for unload (sets the status byte to 0).
    pub fn close(mut self) {
        self.transport.reset();
    }

    /// Borrow the underlying transport mutably for the in-process
    /// software peer to drive on `kick`.
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Allocate one zeroed device-write event buffer, post it to the
    /// eventq, and record it in `event_bufs` under the descriptor head
    /// the queue assigned. The caller is responsible for the single
    /// `kick` once a batch has been posted.
    fn post_slot(
        eventq: &mut SplitQueue,
        event_pool: &BounceBuffer,
        slot: u16,
        event_slots: &mut [Option<u16>],
    ) -> Result<(), DriverError> {
        let offset = u64::from(slot) * u64::from(wire::EVENT_LEN);
        let segments = [ChainSegment {
            phys: event_pool.phys() + offset,
            len: wire::EVENT_LEN,
            direction: Direction::DeviceWrite,
        }];
        let head = eventq
            .add_chain(&segments)
            .map_err(VirtioError::as_driver_error)?;
        // `head` is queue-assigned (the driver's own free list), so it is
        // always in range; guard anyway and fail closed.
        *event_slots
            .get_mut(head as usize)
            .ok_or(DriverError::DeviceFault)? = Some(slot);
        Ok(())
    }
}

impl<T: Transport> Input for VirtioInput<'_, T> {
    fn poll(&mut self, events: &mut [InputEvent]) -> Result<usize, DriverError> {
        if events.is_empty() {
            return Err(DriverError::BufferTooSmall);
        }
        // Drain whatever the device has already completed; only park the
        // CPU (interrupt-driven, never a busy-spin)
        // when nothing is pending, then drain once more. There is no
        // request outstanding to bound this on — the caller is waiting for
        // the *next* keystroke or pointer motion, which may legitimately
        // never come — so this parks indefinitely rather than manufacturing
        // a deadline for an event nothing promised would arrive.
        let mut count = self.drain_ready(events);
        if matches!(count, Ok(0)) {
            self.host.notify_wait(self.eventq.index(), u64::MAX);
            count = self.drain_ready(events);
        }
        // Acknowledge the device's interrupt now that its completions have
        // been observed, so it de-asserts its line before the next wait
        // re-arms the kernel IRQ. Without this the asserted line wakes
        // every subsequent `notify_wait` immediately and the "park" becomes
        // a busy loop through the kernel. Done regardless of whether the
        // drain faulted — a faulted drain must still leave the device's
        // line clear — and a no-op on transports with no device-side ack
        // (MSI-X PCI, the mock). This mirrors `VirtioBlk::run_request`.
        self.transport.ack_interrupt();
        count
    }
}

impl<T: Transport> VirtioInput<'_, T> {
    /// Drain every completed event the device has posted (up to the
    /// caller's `events` capacity), decoding each and immediately
    /// handing its buffer back to the device so the pool stays full.
    ///
    /// Frame separators (`EV_SYN`), unmodelled events, and short
    /// completions consume and replenish a buffer without yielding an
    /// `InputEvent` (fail closed — never decode stale bytes), so a
    /// single keypress's `EV_KEY`+`EV_SYN` pair surfaces exactly one
    /// event.
    fn drain_ready(&mut self, events: &mut [InputEvent]) -> Result<usize, DriverError> {
        let mut count = 0;
        let mut reposted = false;
        while count < events.len() {
            let token = match self.eventq.poll_used() {
                Ok(t) => t,
                Err(VirtioError::NoCompletion) => break,
                Err(e) => return Err(e.as_driver_error()),
            };
            let slot = self
                .event_slots
                .get_mut(token.head as usize)
                .and_then(Option::take)
                .ok_or(DriverError::DeviceFault)?;
            if token.written >= wire::EVENT_LEN {
                let offset = usize::from(slot) * wire::EVENT_LEN as usize;
                let bytes = self
                    .event_pool
                    .full_region_mut()
                    .get(offset..offset + wire::EVENT_LEN as usize)
                    .ok_or(DriverError::DeviceFault)?;
                let etype = u16::from_le_bytes([bytes[0], bytes[1]]);
                let code = u16::from_le_bytes([bytes[2], bytes[3]]);
                let value = i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
                if let Some(event) = decode_event(etype, code, value) {
                    events[count] = event;
                    count += 1;
                }
            }
            Self::post_slot(
                &mut self.eventq,
                &self.event_pool,
                slot,
                &mut self.event_slots,
            )?;
            reposted = true;
        }
        if reposted {
            self.eventq.kick(&mut self.transport);
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests;
