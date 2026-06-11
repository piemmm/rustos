//! xHCI ring state machines (xHCI 1.2 §4.9).
//!
//! Two ring shapes exist: **producer** rings the driver writes and the
//! controller consumes (the command ring and every transfer ring), and
//! the **event** ring the controller writes and the driver consumes.
//! Ownership of each slot is handed over by the cycle bit (§4.9.2):
//! the producer stamps its current cycle state into each TRB it
//! enqueues, the consumer processes TRBs whose cycle bit matches its
//! own state, and each pass over the ring inverts the expectation.
//!
//! The state machines below operate on caller-provided TRB slices —
//! on metal a capability-granted DMA region the controller also sees,
//! in host tests a plain array — so the cycle/wrap/full logic is proven
//! host-side without hardware (`AGENTS.md` §2.2). DMA publication
//! (cache cleaning, address translation) belongs to the seam that owns
//! the memory, not to the ring arithmetic.

use rustos_abi::DriverError;

use crate::trb::{Trb, TrbType, CONTROL_CYCLE, CONTROL_LINK_TOGGLE};

/// A producer ring (command or transfer ring, §4.9.2).
///
/// The last slot permanently holds a Link TRB pointing back to the
/// ring's base with Toggle Cycle set, so the controller follows the
/// wrap and inverts its consumer cycle state in step with the
/// producer's. The producer never lets its enqueue pointer catch the
/// dequeue pointer: the slot *before* the dequeue point stays free, so
/// a full ring is distinguishable from an empty one.
pub struct ProducerRing<'a> {
    trbs: &'a mut [Trb],
    base: u64,
    enqueue: usize,
    dequeue: usize,
    cycle: bool,
}

impl<'a> ProducerRing<'a> {
    /// Initialise a producer ring over `trbs`, whose device-visible
    /// base address is `base` (used as the Link TRB's target and to
    /// report each enqueued TRB's device-visible address).
    ///
    /// Every data slot is cleared to [`Trb::ZERO`] (cycle bit clear —
    /// consumer-owned under the initial producer cycle state of `1`)
    /// and the final slot becomes the Link TRB.
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] if `trbs` has fewer than
    ///   three slots (one link + one data + the mandatory free slot) —
    ///   a smaller ring cannot hold a single in-flight TRB.
    pub fn new(trbs: &'a mut [Trb], base: u64) -> Result<Self, DriverError> {
        if trbs.len() < 3 {
            return Err(DriverError::LengthOutOfRange);
        }
        let link_slot = trbs.len() - 1;
        for trb in &mut trbs[..link_slot] {
            *trb = Trb::ZERO;
        }
        trbs[link_slot] = Trb::new(TrbType::Link, base, 0, CONTROL_LINK_TOGGLE);
        Ok(Self {
            trbs,
            base,
            enqueue: 0,
            dequeue: 0,
            cycle: true,
        })
    }

    /// Data slots in the ring (total minus the Link TRB slot).
    fn data_slots(&self) -> usize {
        self.trbs.len() - 1
    }

    /// Next data-slot index after `slot`, following the wrap.
    fn next_slot(&self, slot: usize) -> usize {
        (slot + 1) % self.data_slots()
    }

    /// TRBs enqueued and not yet retired.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        (self.enqueue + self.data_slots() - self.dequeue) % self.data_slots()
    }

    /// Enqueue `trb`, stamping the producer cycle state into it.
    ///
    /// Returns the device-visible address of the slot the TRB landed
    /// in, which the matching completion event echoes back (§6.4.2).
    /// When the enqueue pointer reaches the Link TRB slot the link's
    /// cycle bit is published last and the producer cycle state
    /// toggles (§4.9.2.1).
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `trb` already carries a cycle
    ///   bit ([`CONTROL_CYCLE`] is ring-owned) or is itself a Link TRB
    ///   (the wrap link is ring-owned).
    /// * [`DriverError::Busy`] if the ring is full: advancing would
    ///   collide with the dequeue point. The caller retires completed
    ///   TRBs ([`Self::retire_one`]) and retries.
    pub fn push(&mut self, trb: Trb) -> Result<u64, DriverError> {
        if trb.control & CONTROL_CYCLE != 0 {
            return Err(DriverError::OutOfRange);
        }
        if matches!(trb.trb_type(), Ok(TrbType::Link)) {
            return Err(DriverError::OutOfRange);
        }
        if self.next_slot(self.enqueue) == self.dequeue {
            return Err(DriverError::Busy);
        }
        let slot = self.enqueue;
        let mut stamped = trb;
        if self.cycle {
            stamped.control |= CONTROL_CYCLE;
        }
        self.trbs[slot] = stamped;
        if self.next_slot(slot) == 0 {
            // Wrapping: publish the Link TRB under the current cycle
            // state (the controller consumes it like any TRB), then
            // toggle ours to match the Toggle Cycle it will perform.
            let link_slot = self.trbs.len() - 1;
            let mut link = self.trbs[link_slot];
            link.control =
                (link.control & !CONTROL_CYCLE) | if self.cycle { CONTROL_CYCLE } else { 0 };
            self.trbs[link_slot] = link;
            self.cycle = !self.cycle;
        }
        self.enqueue = self.next_slot(slot);
        Ok(self.base + (slot * crate::trb::TRB_LEN) as u64)
    }

    /// Retire the oldest in-flight TRB after its completion event.
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if nothing is in flight — a
    ///   completion event for work never enqueued is a controller
    ///   fault, surfaced rather than absorbed (`AGENTS.md` §2.9).
    pub fn retire_one(&mut self) -> Result<(), DriverError> {
        if self.in_flight() == 0 {
            return Err(DriverError::OutOfRange);
        }
        self.dequeue = self.next_slot(self.dequeue);
        Ok(())
    }
}

/// The event-ring consumer cursor (single segment, §4.9.4).
///
/// The controller produces events; the driver consumes every TRB whose
/// cycle bit matches its consumer cycle state, inverting the
/// expectation each time it wraps past the segment end. Event rings
/// have no Link TRBs — the segment table defines the bounds, and this
/// cursor models one segment.
///
/// The cursor holds no borrow of the segment: the controller (or a
/// test) keeps writing new events into the memory while the cursor is
/// live, so [`pop`](Self::pop) takes the segment afresh on every call
/// and validates that its length still matches the registered one.
pub struct EventRingCursor {
    len: usize,
    dequeue: usize,
    cycle: bool,
}

impl EventRingCursor {
    /// Initialise the consumer cursor for a zeroed event segment of
    /// `len` TRBs.
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] if `len` is zero.
    pub fn new(len: usize) -> Result<Self, DriverError> {
        if len == 0 {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok(Self {
            len,
            dequeue: 0,
            cycle: true,
        })
    }

    /// Consume the next event from `trbs`, if the controller has
    /// produced one.
    ///
    /// Returns `Ok(None)` while the TRB at the dequeue point still
    /// carries the previous pass's cycle state (controller-owned).
    /// Consuming the last slot of the segment wraps the dequeue point
    /// and inverts the expected cycle state.
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] if `trbs` is not the
    ///   segment this cursor was created for (its length differs).
    pub fn pop(&mut self, trbs: &[Trb]) -> Result<Option<Trb>, DriverError> {
        if trbs.len() != self.len {
            return Err(DriverError::LengthOutOfRange);
        }
        let trb = trbs[self.dequeue];
        if trb.cycle() != self.cycle {
            return Ok(None);
        }
        self.dequeue += 1;
        if self.dequeue == self.len {
            self.dequeue = 0;
            self.cycle = !self.cycle;
        }
        Ok(Some(trb))
    }

    /// Index of the slot the next event will be consumed from, for the
    /// owner to program the controller's event-ring dequeue pointer.
    #[must_use]
    pub fn dequeue_index(&self) -> usize {
        self.dequeue
    }
}
