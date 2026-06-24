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
//! The state machines below hold no memory: the producer ring returns
//! the stamped TRBs to publish and the event cursor consumes from a
//! caller-provided snapshot. The owner of the device-shared memory —
//! on metal a capability-granted DMA region, in host tests a plain
//! buffer — performs every read and write, so the cycle/wrap/full
//! logic is proven host-side without hardware (`AGENTS.md` §2.2) and
//! DMA publication (cache cleaning, address translation, write
//! ordering) stays with the seam that owns the memory.

use rustos_abi::DriverError;

use crate::trb::{Trb, TrbType, CONTROL_CYCLE, CONTROL_LINK_TOGGLE};

/// What one [`ProducerRing::push`] obliges the memory owner to
/// publish.
///
/// `trb` is published at `slot`; when `link` is `Some`, the updated
/// Link TRB is published at the ring's final slot **after** `trb`
/// (the controller must never observe the re-cycled link ahead of the
/// data TRB it follows, §4.9.2.1).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PushOutcome {
    /// Data-slot index the TRB landed in.
    pub slot: usize,
    /// Device-visible address of that slot, which the matching
    /// completion event echoes back (§6.4.2).
    pub address: u64,
    /// The cycle-stamped TRB to publish at `slot`.
    pub trb: Trb,
    /// The updated Link TRB to publish at the final slot, when this
    /// push wrapped the ring.
    pub link: Option<Trb>,
}

/// A producer ring (command or transfer ring, §4.9.2).
///
/// The last slot permanently holds a Link TRB pointing back to the
/// ring's base with Toggle Cycle set, so the controller follows the
/// wrap and inverts its consumer cycle state in step with the
/// producer's. The producer never lets its enqueue pointer catch the
/// dequeue pointer: the slot *before* the dequeue point stays free, so
/// a full ring is distinguishable from an empty one.
pub struct ProducerRing {
    len: usize,
    base: u64,
    enqueue: usize,
    dequeue: usize,
    cycle: bool,
}

impl ProducerRing {
    /// Initialise a producer ring of `len` slots whose device-visible
    /// base address is `base`.
    ///
    /// Returns the ring and the initial Link TRB (cycle bit clear —
    /// consumer-owned under the initial producer cycle state of `1`)
    /// the memory owner publishes at slot `len - 1` after zeroing the
    /// data slots to [`Trb::ZERO`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] if `len` is below three
    ///   slots (one link + one data + the mandatory free slot) — a
    ///   smaller ring cannot hold a single in-flight TRB.
    pub fn new(len: usize, base: u64) -> Result<(Self, Trb), DriverError> {
        if len < 3 {
            return Err(DriverError::LengthOutOfRange);
        }
        let link = Trb::new(TrbType::Link, base, 0, CONTROL_LINK_TOGGLE);
        Ok((
            Self {
                len,
                base,
                enqueue: 0,
                dequeue: 0,
                cycle: true,
            },
            link,
        ))
    }

    /// Slot index of the permanent Link TRB (the final slot).
    #[must_use]
    pub fn link_slot(&self) -> usize {
        self.len - 1
    }

    /// Data-slot index the next [`Self::push`] will land in, for
    /// owners that pair a per-slot buffer with the TRB before pushing.
    #[must_use]
    pub fn enqueue_slot(&self) -> usize {
        self.enqueue
    }

    /// Data slots in the ring (total minus the Link TRB slot).
    fn data_slots(&self) -> usize {
        self.len - 1
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
    /// Returns the [`PushOutcome`] the memory owner publishes. When
    /// the enqueue pointer reaches the Link TRB slot the outcome also
    /// carries the re-cycled Link TRB (published after the data TRB)
    /// and the producer cycle state toggles (§4.9.2.1).
    ///
    /// # Errors
    ///
    /// * [`DriverError::OutOfRange`] if `trb` already carries a cycle
    ///   bit ([`CONTROL_CYCLE`] is ring-owned) or is itself a Link TRB
    ///   (the wrap link is ring-owned).
    /// * [`DriverError::Busy`] if the ring is full: advancing would
    ///   collide with the dequeue point. The caller retires completed
    ///   TRBs ([`Self::retire_one`]) and retries.
    pub fn push(&mut self, trb: Trb) -> Result<PushOutcome, DriverError> {
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
        let link = if self.next_slot(slot) == 0 {
            // Wrapping: the Link TRB is re-published under the current
            // cycle state (the controller consumes it like any TRB),
            // then the producer cycle toggles to match the Toggle
            // Cycle the controller will perform.
            let link = Trb::new(
                TrbType::Link,
                self.base,
                0,
                CONTROL_LINK_TOGGLE | if self.cycle { CONTROL_CYCLE } else { 0 },
            );
            self.cycle = !self.cycle;
            Some(link)
        } else {
            None
        };
        self.enqueue = self.next_slot(slot);
        Ok(PushOutcome {
            slot,
            address: self.base + (slot * crate::trb::TRB_LEN) as u64,
            trb: stamped,
            link,
        })
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

    /// Whether the controller has produced the event at the dequeue point —
    /// the slot's cycle bit matches the consumer cycle state — **without**
    /// advancing the cursor.
    ///
    /// The owner reads this from a first snapshot of the segment to decide
    /// whether an event is present, issues a DMA read barrier, then re-reads
    /// the segment and [`pop`](Self::pop)s, so the entry body is observed
    /// after the cycle bit that announced it (the controller writes the body
    /// before the cycle bit; on non-coherent DMA memory an unordered read of
    /// the whole entry can pair a fresh cycle bit with a stale body).
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] if `trbs` is not the segment this
    ///   cursor was created for (its length differs).
    pub fn owned(&self, trbs: &[Trb]) -> Result<bool, DriverError> {
        if trbs.len() != self.len {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok(trbs[self.dequeue].cycle() == self.cycle)
    }

    /// Index of the slot the next event will be consumed from, for the
    /// owner to program the controller's event-ring dequeue pointer.
    #[must_use]
    pub fn dequeue_index(&self) -> usize {
        self.dequeue
    }
}
