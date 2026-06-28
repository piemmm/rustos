//! The HCD's per-interface URB-service state machine and interface-node
//! builder (`plans/USB.md` §1.1, §1.3 — the asynchronous event loop).
//!
//! The host-controller driver serves one URB transport call endpoint per USB
//! interface it emits. A class driver submits an interrupt-IN URB (a blocking
//! `ipc_call`) to read the next report; the HCD does **not** reply until the
//! controller's completion interrupt delivers that report, so the class driver
//! parks in the kernel rather than busy-polling (the charter forbids spinning
//! a core). [`UrbService`] is the per-interface state that makes this work: it
//! holds at most one outstanding URB and drives it on the controller event.
//!
//! The data path is the U3a2 shared-memory buffer: the report bytes the
//! controller wrote into the HCD's own DMA ring are copied into the shared
//! buffer (the `shm` slice here — the HCD's mapping of the region the class
//! driver also maps) by the engine's [`interrupt_in`](UrbEngine::interrupt_in),
//! and the class driver reads them from its own mapping. The class driver
//! holds no DMA grant.
//!
//! This module is pure and alloc-free, so it is proven host-side over a mock
//! [`UrbEngine`]; the live wait-set loop that drives it is in `main.rs` and is
//! the on-metal acceptance item (QEMU models no Pi USB).

use rustos_abi::usb_urb::{URB_COMPLETION_LEN, URB_REQUEST_LEN};
use rustos_abi::{DriverError, Errno, HwNode, HwResource};
use rustos_usb::transport::{drive_urb, frame_completion, UrbEngine};

/// A framed URB completion ready for `call_reply`: the in-band completion
/// bytes and their length, paired with the ticket the reply answers.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct UrbReply {
    /// The in-service call ticket this reply answers.
    pub ticket: u64,
    /// The framed completion bytes.
    pub bytes: [u8; URB_COMPLETION_LEN],
    /// The number of valid bytes in [`Self::bytes`].
    pub len: usize,
}

/// What the HCD does after servicing one wait-set wake-up.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UrbOutcome {
    /// Reply now to the named ticket with the framed completion.
    Reply(UrbReply),
    /// The submitted URB is held outstanding; no reply is sent until a later
    /// controller event completes it.
    Held,
    /// A controller event arrived but no URB is outstanding — nothing to do.
    Idle,
}

/// One USB interface's URB-service state: at most one outstanding interrupt-IN
/// URB.
///
/// The class driver submits one URB at a time (it blocks on the reply), so a
/// single outstanding slot is the whole protocol. A second submit arriving
/// while one is already outstanding is a class-driver protocol violation; it
/// is answered fail-closed with [`Errno::AlreadyExists`] and never displaces
/// the URB in flight (a hostile class driver cannot steal another submit's
/// completion).
pub struct UrbService {
    /// The in-flight URB: its `call_recv` ticket and the fixed-size request
    /// frame, re-driven on each controller event. `None` when idle.
    outstanding: Option<(u64, [u8; URB_REQUEST_LEN], usize)>,
}

impl Default for UrbService {
    fn default() -> Self {
        Self::new()
    }
}

impl UrbService {
    /// A service with no URB outstanding.
    #[must_use]
    pub const fn new() -> Self {
        Self { outstanding: None }
    }

    /// Whether a URB is currently in flight (a controller event will drive
    /// it).
    #[must_use]
    pub const fn is_busy(&self) -> bool {
        self.outstanding.is_some()
    }

    /// Frame `result` into a [`UrbReply`] for `ticket`.
    ///
    /// Framing a completion into a [`URB_COMPLETION_LEN`] buffer cannot fail
    /// (the buffer is exactly the sized destination), so the only outcome is
    /// the framed length; a defensive `unwrap_or(0)` keeps this panic-free on
    /// any path rather than relying on that invariant.
    fn reply(ticket: u64, result: Result<u32, Errno>) -> UrbReply {
        let mut bytes = [0u8; URB_COMPLETION_LEN];
        let len = frame_completion(&mut bytes, result).unwrap_or(0);
        UrbReply { ticket, bytes, len }
    }

    /// Service a freshly received URB (`ticket` + its `request` frame) over the
    /// shared `shm` buffer and the controller `engine`.
    ///
    /// Drives the URB once: a transfer that completes synchronously (a control
    /// transfer, or an interrupt-IN report already queued) is
    /// [`UrbOutcome::Reply`]-now; an interrupt-IN report not yet arrived is
    /// [`UrbOutcome::Held`] (the controller event will complete it); a
    /// malformed or illegal URB is answered fail-closed. If the interface node
    /// is not live, a submit is rejected [`Errno::NotFound`] without touching
    /// the controller. A submit arriving while a URB is already outstanding is
    /// rejected [`Errno::AlreadyExists`] without disturbing the in-flight URB.
    pub fn on_submit<E: UrbEngine>(
        &mut self,
        interface_live: bool,
        ticket: u64,
        request: &[u8],
        shm: &mut [u8],
        engine: &mut E,
    ) -> UrbOutcome {
        if !interface_live {
            return UrbOutcome::Reply(Self::reply(ticket, Err(Errno::NotFound)));
        }
        if self.outstanding.is_some() {
            return UrbOutcome::Reply(Self::reply(ticket, Err(Errno::AlreadyExists)));
        }
        match drive_urb(request, shm, engine) {
            Ok(Some(transferred)) => UrbOutcome::Reply(Self::reply(ticket, Ok(transferred))),
            Ok(None) => {
                // Latch the fixed-size request frame so the controller event
                // can re-drive it. A request longer than the frame cannot
                // occur (`drive_urb` decoded it from exactly `URB_REQUEST_LEN`
                // bytes), but clamp defensively.
                let mut frame = [0u8; URB_REQUEST_LEN];
                let n = request.len().min(URB_REQUEST_LEN);
                frame[..n].copy_from_slice(&request[..n]);
                self.outstanding = Some((ticket, frame, n));
                UrbOutcome::Held
            }
            Err(err) => UrbOutcome::Reply(Self::reply(ticket, Err(err))),
        }
    }

    /// Service a controller event over the shared `shm` buffer and `engine`.
    ///
    /// If a URB is outstanding it is re-driven: a completed transfer is
    /// [`UrbOutcome::Reply`]-now (clearing the slot); a still-pending
    /// interrupt-IN report leaves it [`UrbOutcome::Held`]; a fault is answered
    /// fail-closed (clearing the slot). With nothing outstanding the event is
    /// [`UrbOutcome::Idle`] (e.g. a PORTSC change the caller handles
    /// separately).
    pub fn on_event<E: UrbEngine>(&mut self, shm: &mut [u8], engine: &mut E) -> UrbOutcome {
        let Some((ticket, frame, len)) = self.outstanding.take() else {
            return UrbOutcome::Idle;
        };
        match drive_urb(&frame[..len], shm, engine) {
            Ok(Some(transferred)) => UrbOutcome::Reply(Self::reply(ticket, Ok(transferred))),
            Ok(None) => {
                // Still no report — keep the URB outstanding for the next event.
                self.outstanding = Some((ticket, frame, len));
                UrbOutcome::Held
            }
            Err(err) => UrbOutcome::Reply(Self::reply(ticket, Err(err))),
        }
    }

    /// Abort the in-flight URB, if any, with `errno` and clear the service for
    /// the next class-driver instance.
    ///
    /// A device disconnect can unload a class driver while its blocking
    /// interrupt-IN request is still parked in the kernel. The HCD keeps
    /// serving the same endpoint across a later replug, so the stale request
    /// must not survive and block the freshly loaded class driver.
    #[must_use]
    pub fn abort_outstanding(&mut self, errno: Errno) -> UrbOutcome {
        let Some((ticket, _, _)) = self.outstanding.take() else {
            return UrbOutcome::Idle;
        };
        UrbOutcome::Reply(Self::reply(ticket, Err(errno)))
    }
}

/// Extend the enumerated device's interface [`HwNode`] (from
/// [`describe_device`](rustos_usb::device::UsbDevice::describe_device)) with
/// the URB-transport grants the autoloaded class driver inherits: the
/// per-endpoint call grant for `endpoint_id` and the per-region shared-memory
/// grant for `shm_id`.
///
/// The node already carries the USB `vid:pid:class` match keys; adding these
/// two resources is what lets the kernel mint the class driver exactly the
/// authority to submit URBs on this one interface and to map this one shared
/// buffer — and no controller register, no DMA, no other interface's buffer
/// (least privilege). The kernel's `hw_emit_node` coverage check admits the
/// node because the HCD holds both grants (minted when it created the endpoint
/// and the region).
///
/// # Errors
///
/// [`DriverError::NoSpace`] if the node cannot carry both grants.
pub fn attach_transport_grants(
    mut node: HwNode,
    endpoint_id: u64,
    shm_id: u64,
) -> Result<HwNode, DriverError> {
    node.push_resource(HwResource::endpoint(endpoint_id))
        .map_err(|_| DriverError::NoSpace)?;
    node.push_resource(HwResource::shared(shm_id))
        .map_err(|_| DriverError::NoSpace)?;
    Ok(node)
}

#[cfg(test)]
#[path = "serve_tests.rs"]
mod tests;
