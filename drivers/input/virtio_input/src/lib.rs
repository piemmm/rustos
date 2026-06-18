//! RustOS virtio-input driver (keyboard / pointer).
//!
//! Implements [`rustos_abi::driver::input::Input`] on top of the
//! cross-arch virtio transport from `lib/virtio`. As with
//! `virtio_blk` / `virtio_net`, the driver is bus-agnostic: the same
//! source compiles against the PCI and MMIO transports (`AGENTS.md`
//! §2.2 — the queue protocol lives once, in the transport crate).
//!
//! # Wire protocol
//!
//! Virtio 1.1 §5.8. A virtio-input device exposes two virtqueues —
//! the **eventq** (index 0), on which the device delivers
//! `struct virtio_input_event` records to the driver, and the
//! **statusq** (index 1), on which the driver returns status (LEDs,
//! force-feedback) to the device. This driver consumes the eventq
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
//! event-path driver does not use), so the driver accepts the empty
//! feature subset.
//!
//! # Public surface
//!
//! Per `AGENTS.md` §8 the only public *function* is [`register`].
//! [`VirtioInput`] is a public *type* re-exported so the driver host
//! can instantiate it; the host never reaches the type beyond the
//! [`Input`] trait. [`BIND_KEYS`] is the §18.3 bind table `devmgr`
//! (or the in-kernel bootstrap-floor catalogue) resolves a discovered
//! virtio-input node against.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]; `poll` requires no
//! further per-method capability (the dispatcher routes decoded
//! events to the focused session — see `lib/abi/src/driver/input.rs`).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use rustos_abi::driver::input::{Input, InputEvent, InputEventKind};
use rustos_abi::driver::BufferClass;
use rustos_abi::{CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey};
use rustos_virtio::{
    BounceBuffer, ChainSegment, Direction, SplitQueue, Status, Transport, VirtioError, VirtioHost,
};

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x564E_5054_0000_0001; // "VNPT" (Virtio iNPuT)

/// The virtio device id of an input device (virtio 1.1 §5.8 —
/// `virtio-input` is device type 18). This driver's [`BIND_KEYS`] match
/// key is built from it, so a discovered virtio node whose probed device
/// id is 18 binds this driver and nothing else.
pub const VIRTIO_INPUT_DEVICE_ID: u32 = 18;

/// The §18.3 bind priority [`BIND_KEYS`] carries.
///
/// A virtio device-id match is *exact* (the discovered node's probed
/// device id either is `virtio-input` or it is not — there is no
/// wildcard, see [`HwMatchKey::matches`]), so it ranks at the
/// exact-match tier alongside the other concrete-identity drivers
/// (`AGENTS.md` §18.3 — higher matched priority binds; an unbroken tie
/// is a packaging defect).
const BIND_PRIORITY: u16 = 10;

/// This driver's hardware bind table (`AGENTS.md` §18.3): a virtio input
/// device, matched by its virtio device id ([`VIRTIO_INPUT_DEVICE_ID`]).
///
/// The single source of truth the signed-manifest bind table is authored
/// from and `devmgr` (or the in-kernel bootstrap-floor catalogue)
/// resolves a discovered node against (`AGENTS.md` §2.2 / §18.3). The
/// match key carries no transport (PCI vs MMIO) detail: the same driver
/// binds a virtio-input device however it is attached, because the
/// bus-agnostic [`Transport`] abstracts the transport (`AGENTS.md` §2.2 /
/// §17.4).
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    HwMatchKey::virtio(VIRTIO_INPUT_DEVICE_ID),
)];

/// Driver entry point (`AGENTS.md` §8).
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

/// Virtio-input wire-protocol constants (virtio 1.1 §5.8 + the Linux
/// `evdev` `type`/`code` namespaces the device reports in).
mod wire {
    /// Event virtqueue index (device → driver), virtio 1.1 §5.8.2.
    pub const EVENT_QUEUE: u16 = 0;
    /// Event-queue size (descriptors). Power-of-two per virtio §2.6;
    /// eight outstanding single-event buffers is ample headroom for the
    /// one-event-per-`poll` drain below.
    pub const EVENT_QUEUE_SIZE: u16 = 8;
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

    /// `code` value for the X axis in the platform-neutral
    /// [`InputEventKind::Pointer`](rustos_abi::driver::input::InputEventKind::Pointer)
    /// / `Scroll` encoding.
    pub const AXIS_X: u16 = 0;
    /// `code` value for the Y axis in the platform-neutral
    /// pointer / scroll encoding.
    pub const AXIS_Y: u16 = 1;
}

/// Decode one raw `virtio_input_event` triple into the platform-neutral
/// [`InputEvent`].
///
/// Returns `None` for frame markers (`EV_SYN`) and any `type`/`code`
/// this `abi-v1` input surface does not model, so the caller treats a
/// consumed-but-unmapped event as "no event" rather than fabricating a
/// bogus one (`AGENTS.md` §2.9 — fail closed, never guess).
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
                code: wire::AXIS_X,
                value,
            }),
            wire::REL_Y => Some(InputEvent {
                kind: InputEventKind::Pointer,
                reserved0: 0,
                code: wire::AXIS_Y,
                value,
            }),
            wire::REL_WHEEL => Some(InputEvent {
                kind: InputEventKind::Scroll,
                reserved0: 0,
                code: wire::AXIS_Y,
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
/// for `'h` rather than demanding a `'static` host (`AGENTS.md` §4 —
/// per-process pools are reclaimed when the driver unloads). This
/// mirrors [`VirtioNet`](../rustos_drv_network_virtio_net/struct.VirtioNet.html).
pub struct VirtioInput<'h, T: Transport> {
    transport: T,
    eventq: SplitQueue,
    host: &'h dyn VirtioHost,
    /// The pool of device-writable event buffers, indexed by the
    /// descriptor head the queue assigned each one. The device fills a
    /// buffer per `virtio_input_event` it delivers; the driver keeps the
    /// whole pool posted so several events (e.g. an `EV_KEY` plus its
    /// `EV_SYN` frame separator) can be in flight at once — a single
    /// posted buffer is not enough, because the device needs a free
    /// buffer for *every* event of a report, including the `EV_SYN`
    /// (virtio 1.1 §5.8.6).
    event_bufs: [Option<BounceBuffer>; wire::EVENT_QUEUE_SIZE as usize],
}

impl<'h, T: Transport> VirtioInput<'h, T> {
    /// Bring the device online and post the event-buffer pool.
    ///
    /// Implements the virtio-1.1 §3.1 initialisation sequence: reset,
    /// ACKNOWLEDGE, DRIVER, feature negotiation (`VIRTIO_F_VERSION_1`
    /// only — the modern split-virtqueue layout, no device-specific
    /// features), `FEATURES_OK`, set up the event queue, `DRIVER_OK`,
    /// then fill the eventq with `EVENT_QUEUE_SIZE` device-write
    /// buffers and notify the device.
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
        let mut eventq = SplitQueue::new(
            &mut transport,
            host,
            wire::EVENT_QUEUE,
            wire::EVENT_QUEUE_SIZE,
        )
        .map_err(VirtioError::as_driver_error)?;
        status = status.with(Status::DRIVER_OK);
        transport.set_status(status);

        let mut event_bufs: [Option<BounceBuffer>; wire::EVENT_QUEUE_SIZE as usize] =
            core::array::from_fn(|_| None);
        for _ in 0..wire::EVENT_QUEUE_SIZE {
            Self::post_buffer(&mut eventq, host, &mut event_bufs)?;
        }
        eventq.kick(&mut transport);

        Ok(Self {
            transport,
            eventq,
            host,
            event_bufs,
        })
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
    fn post_buffer(
        eventq: &mut SplitQueue,
        host: &dyn VirtioHost,
        event_bufs: &mut [Option<BounceBuffer>],
    ) -> Result<(), DriverError> {
        let region = host.alloc_dma_zeroed(wire::EVENT_LEN as usize)?;
        let buf = BounceBuffer::new(region, BufferClass::NonSensitive);
        let segments = [ChainSegment {
            phys: buf.phys(),
            len: wire::EVENT_LEN,
            direction: Direction::DeviceWrite,
        }];
        let head = eventq
            .add_chain(&segments)
            .map_err(VirtioError::as_driver_error)?;
        // `head` is queue-assigned (the driver's own free list), so it is
        // always in range; guard anyway and fail closed (§5.4).
        *event_bufs
            .get_mut(head as usize)
            .ok_or(DriverError::DeviceFault)? = Some(buf);
        Ok(())
    }
}

impl<T: Transport> Input for VirtioInput<'_, T> {
    fn poll(&mut self, events: &mut [InputEvent]) -> Result<usize, DriverError> {
        if events.is_empty() {
            return Err(DriverError::BufferTooSmall);
        }
        // Drain whatever the device has already completed; only park the
        // CPU (interrupt-driven, never a busy-spin — `AGENTS.md` §2.1)
        // when nothing is pending, then drain once more.
        let mut count = self.drain_ready(events)?;
        if count == 0 {
            self.host.notify_wait(self.eventq.index());
            count = self.drain_ready(events)?;
        }
        Ok(count)
    }
}

impl<T: Transport> VirtioInput<'_, T> {
    /// Drain every completed event the device has posted (up to the
    /// caller's `events` capacity), decoding each and immediately
    /// handing its buffer back to the device so the pool stays full.
    ///
    /// Frame separators (`EV_SYN`), unmodelled events, and short
    /// completions consume and replenish a buffer without yielding an
    /// `InputEvent` (fail closed — never decode stale bytes, §5.4), so a
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
            let mut buf = self
                .event_bufs
                .get_mut(token.head as usize)
                .and_then(Option::take)
                .ok_or(DriverError::DeviceFault)?;
            if token.written >= wire::EVENT_LEN {
                let bytes = buf.full_region_mut();
                let etype = u16::from_le_bytes([bytes[0], bytes[1]]);
                let code = u16::from_le_bytes([bytes[2], bytes[3]]);
                let value = i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
                if let Some(event) = decode_event(etype, code, value) {
                    events[count] = event;
                    count += 1;
                }
            }
            let segments = [ChainSegment {
                phys: buf.phys(),
                len: wire::EVENT_LEN,
                direction: Direction::DeviceWrite,
            }];
            let head = self
                .eventq
                .add_chain(&segments)
                .map_err(VirtioError::as_driver_error)?;
            *self
                .event_bufs
                .get_mut(head as usize)
                .ok_or(DriverError::DeviceFault)? = Some(buf);
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
