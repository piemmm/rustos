//! The console-output gate: the one serialisation point every producer and
//! every drainer of a port's character console passes through.
//!
//! # Why a gate and not just a queue
//!
//! A console has many producers — the diagnostic log sink, a program writing
//! its own output, an interrupt handler reporting a device fault — and several
//! drainers: the producer itself, a transmit-completion interrupt, and the
//! dispatch loop. All of them touch the same [`OutQueue`], so all of them must
//! be serialised, and they must be serialised in a way that is correct when the
//! *same* CPU is both.
//!
//! That last point is the crux. TAIRiX takes device interrupts while in-kernel
//! code runs, so a drain step running in a dispatch loop can be interrupted by
//! a handler that logs. If the drainer held the queue with interrupts enabled,
//! that handler would wait on a lock its own interrupted mainline holds — a
//! single-CPU self-deadlock — and any "give up and write straight to the
//! device" escape from that wait splices the handler's bytes into the middle of
//! the line the queue is transmitting. Corrupted output is the symptom; the
//! unmasked hold is the cause.
//!
//! So the gate takes the queue through [`IrqSafeSpinLock`], which masks the
//! current CPU's interrupts for the whole hold — the standing discipline for
//! any lock an interrupt handler and a normal path share. Every acquirer, drain
//! step included, masks. Same-CPU re-entrancy then cannot happen at all, and
//! contention can only ever come from another CPU, which releases within a
//! bounded hold.
//!
//! # What the gate guarantees
//!
//! * **Whole lines.** A record is rendered into one frame and admitted
//!   atomically, so two producers' bytes never interleave inside a line and a
//!   partial line never reaches the wire.
//! * **No waiting on the device from a hot path.** A producer hands its bytes
//!   to the queue and moves what the transmitter will take *right now*. Where
//!   the transmitter can raise a completion interrupt, the backlog drains in
//!   the background at the device's own rate.
//! * **Loss is reported.** Output the queue could not accept is counted and
//!   emitted as a real diagnostic record at the exact position in the stream
//!   where the gap occurred, so a truncated capture can never be mistaken for
//!   the whole truth.
//! * **One format.** The gate renders every record, so a port supplies only its
//!   device primitives and never a second line formatter.

use core::fmt::Write;

use tairix_log::field::FieldValue;
use tairix_log::{write_diag_line, Event, Field, Level};
use tairix_sync::irq::InterruptControl;
use tairix_sync::{IrqSafeSpinLock, IrqSafeSpinLockGuard};

use crate::events::CONSOLE_OUTPUT_DROPPED;
use crate::queue::{Admit, Class, OutQueue};

/// Bytes handed to a bounded-waiting transmitter in one go.
///
/// Sized to a typical transmit FIFO: enough that a burst is one filling of the
/// device rather than a byte at a time, small enough that the gate is never
/// held — and interrupts never masked — across a long transmission.
const TX_BURST_BYTES: usize = 16;

/// Free body bytes the queue must have before a loss report is rendered.
///
/// A report is one short line, but its two counters can each be a full 20-digit
/// `u64`. Reserving comfortably more than the longest possible report means the
/// attempt only happens when it will succeed, so a persistently full queue does
/// not inflate its own loss count with failed reports.
const LOSS_REPORT_RESERVE_BYTES: usize = 160;

/// Probes a producer spends waiting for another CPU to release the gate before
/// it writes straight to the device instead.
///
/// Every holder masks its own interrupts and holds only long enough to render
/// one line or fill the transmit FIFO, so this budget is orders of magnitude
/// more than a live holder needs. Exhausting it means the holder is not going
/// to release — a CPU died inside the section — and losing the record is worse
/// than bypassing the queue, which is why the escape exists at all.
const GATE_PROBE_BUDGET: u32 = 1 << 20;

/// A port's console transmitter.
///
/// The whole of a port's device knowledge: how to hand bytes to the hardware,
/// whether the hardware can say "I am ready for more", and what time it is.
/// Everything else — framing, admission, loss accounting, locking, rendering —
/// is the gate's, so a new port implements only this.
pub trait ConsoleTx {
    /// Whether this transmitter raises an interrupt as it drains, *and* the
    /// port has wired that interrupt to [`ConsoleGate::service_completion`].
    ///
    /// With a completion interrupt the gate defers the backlog to it, so a
    /// producer never waits on the device. Without one there is no later event
    /// to finish the transmission, so a producer drains what it queued before
    /// returning — bounded, in bursts, releasing the gate between them.
    const COMPLETION_INTERRUPT: bool;

    /// Milliseconds since boot for the record stamp, or `None` on a port with
    /// no monotonic time source yet.
    fn uptime_ms(&self) -> Option<u64>;

    /// Hand over as many leading bytes of `bytes` as the transmitter accepts
    /// **right now**, returning the count. Must not wait, spin, or block.
    fn send_ready(&self, bytes: &[u8]) -> usize;

    /// Hand over as many leading bytes of `bytes` as possible, waiting
    /// **boundedly** for room, and return the count transmitted.
    ///
    /// A count short of `bytes.len()` means the transmitter is not draining;
    /// the gate then discards the rest of that line rather than resuming it
    /// mid-way later.
    fn send_bounded(&self, bytes: &[u8]) -> usize;

    /// Arm or disarm the transmit-completion interrupt.
    ///
    /// Called only by a port that reports [`Self::COMPLETION_INTERRUPT`],
    /// which is why the default does nothing.
    fn set_completion_interrupt(&self, armed: bool) {
        let _ = armed;
    }

    /// Write one byte directly to the device, waiting boundedly, and report
    /// whether the transmitter took it.
    ///
    /// The escape used only when the gate cannot be acquired because a CPU died
    /// holding it. Losing the record entirely would be worse.
    fn send_bypass(&self, byte: u8) -> bool;
}

/// The serialised console output path for one device.
///
/// `CAP` is the queue capacity in bytes and `C` the port's interrupt-control
/// primitives, which the gate masks with for every hold.
pub struct ConsoleGate<T: ConsoleTx, C: InterruptControl, const CAP: usize> {
    /// The framed queue, reachable only with the current CPU's interrupts
    /// masked.
    queue: IrqSafeSpinLock<OutQueue<CAP>, C>,
    /// The port's device primitives.
    tx: T,
}

impl<T: ConsoleTx, C: InterruptControl, const CAP: usize> ConsoleGate<T, C, CAP> {
    /// A gate over `tx` with an empty queue, constructible as a `static` so the
    /// very first boot record already goes through it.
    pub const fn new(tx: T) -> Self {
        Self {
            queue: IrqSafeSpinLock::new(OutQueue::new()),
            tx,
        }
    }

    /// The port's transmitter, for the device-level work only the port itself
    /// can do (bringing the device up, servicing a receive interrupt).
    pub const fn tx(&self) -> &T {
        &self.tx
    }

    /// Render `event` as one whole line and move output toward the device.
    ///
    /// The line is rendered in the shared diagnostic shape, with `\n`
    /// translated to `\r\n` so a serial capture renders line breaks. A line
    /// that does not fit displaces queued records of lower severity; failing
    /// that it is refused whole and counted, never half-written.
    pub fn write_event(&self, event: &Event<'_>) {
        let Some(mut guard) = self.acquire() else {
            self.bypass_event(event);
            return;
        };
        let queue = &mut *guard;
        self.report_loss(queue);
        let uptime = self.tx.uptime_ms();
        if Self::render(queue, uptime, event) == Admit::Refused {
            let shortfall = queue.shortfall(Self::measure(uptime, event));
            if queue.evict_tail_below(event.level, shortfall) {
                Self::render(queue, uptime, event);
            }
        }
        self.advance(guard);
    }

    /// Buffer `bytes` of a program's own output verbatim, returning how many
    /// were accepted.
    ///
    /// Program output is data, not diagnostics: it is never dropped to make
    /// room and never discarded silently. When the queue cannot take all of it
    /// the count is short and the caller — ultimately the writing program —
    /// learns exactly what was taken.
    ///
    /// A *zero* return is reserved for a console that genuinely cannot take a
    /// byte, because a caller reading it as "the sink has stalled" is right to.
    /// So when the queue is full this drains to the device — boundedly, a burst
    /// at a time, releasing the gate between bursts — until there is room. That
    /// wait is what bounds the queue's memory, and it falls on the writing
    /// program, never on a diagnostic path.
    ///
    /// It terminates on any device: each burst either moves bytes toward
    /// retiring a queued line, or finds the transmitter not draining and
    /// abandons that line (counted), which frees the room outright.
    #[must_use]
    pub fn write_output(&self, bytes: &[u8]) -> usize {
        loop {
            let accepted = self.admit_output(bytes);
            if accepted != 0 || bytes.is_empty() {
                return accepted;
            }
            if !self.drain_burst() {
                // Nothing queued yet still no room, or the gate is
                // unreachable: there is no progress left to make.
                return 0;
            }
        }
    }

    /// One attempt to take a prefix of `bytes` into the queue.
    fn admit_output(&self, bytes: &[u8]) -> usize {
        let Some(mut guard) = self.acquire() else {
            let mut bypassed = 0;
            for &byte in bytes {
                if !self.tx.send_bypass(byte) {
                    break;
                }
                bypassed += 1;
            }
            return bypassed;
        };
        let queue = &mut *guard;
        self.report_loss(queue);
        let accepted = queue.admit_prefix(Class::Stream, bytes);
        self.advance(guard);
        accepted
    }

    /// One non-blocking step of moving queued output to the device: push what
    /// the transmitter will take right now and leave the rest to the completion
    /// interrupt.
    ///
    /// Safe on any path — a hot one, an interrupt handler, the dispatch loop —
    /// because it never waits on the device. A momentarily contended gate is a
    /// no-op: the holder performs the same step on its way out.
    pub fn pump(&self) {
        if let Some(mut guard) = self.queue.try_lock() {
            self.step(&mut guard);
        }
    }

    /// Service a transmit-completion interrupt: refill the device and re-arm
    /// the interrupt to whatever is left.
    ///
    /// Identical to [`Self::pump`] — the completion interrupt and the dispatch
    /// loop want exactly the same non-blocking step, so there is one.
    pub fn service_completion(&self) {
        self.pump();
    }

    /// Drain the queue to the device, waiting boundedly, and return when it is
    /// empty or the transmitter has stopped draining.
    ///
    /// Reserved for paths that are about to stop running the dispatch loop — a
    /// panic bridge, where the buffered context that led to the failure must
    /// reach a capture before the CPU parks and blocking can no longer starve
    /// anything — and for ports whose transmitter cannot raise a completion
    /// interrupt. The gate is released between bursts, so interrupts are never
    /// masked across a long transmission.
    pub fn flush(&self) {
        while self.drain_burst() {}
    }

    /// Move one burst to the device, waiting boundedly for it, and report
    /// whether the queue still holds output.
    ///
    /// The one place this crate waits on a device. It is bounded twice over:
    /// the burst is a FIFO-sized run of bytes, and the transmitter's own wait
    /// gives up on a device that is not draining. The gate is released
    /// afterwards, so interrupts are never masked across a long transmission.
    fn drain_burst(&self) -> bool {
        // Bounded-wait rather than skip on contention: on a port with no
        // completion interrupt this is the *only* thing that moves bytes, so
        // abandoning the drain because another CPU held the gate for a moment
        // could leave the last line sitting in memory until the next producer
        // happened along. A live holder releases within one burst; a holder
        // that never does has died in the section, and then giving up is
        // right.
        let Some(mut queue) = self.acquire() else {
            return false;
        };
        let run = queue.peek();
        if run.is_empty() {
            self.arm(false);
            return false;
        }
        let attempted = run.len().min(TX_BURST_BYTES);
        let sent = self.tx.send_bounded(&run[..attempted]);
        queue.consume(sent);
        if sent < attempted {
            // The transmitter is not draining. Abandoning the rest of this
            // line keeps the wire resuming at a line boundary if the device
            // recovers, and the discard is counted like any other loss.
            queue.discard_head_frame();
        }
        true
    }

    /// Acquire the gate, or report that a CPU is stuck holding it.
    ///
    /// Bounded rather than unbounded because a console must keep working while
    /// the system is dying: a holder that never releases has died inside the
    /// section, and the caller's escape is to write straight to the device.
    fn acquire(&self) -> Option<IrqSafeSpinLockGuard<'_, OutQueue<CAP>, C>> {
        for _ in 0..GATE_PROBE_BUDGET {
            if let Some(guard) = self.queue.try_lock() {
                return Some(guard);
            }
            core::hint::spin_loop();
        }
        None
    }

    /// Finish a producer's call: move output to the device, and where the
    /// transmitter cannot tell us it has drained, keep going until it has.
    fn advance(&self, mut guard: IrqSafeSpinLockGuard<'_, OutQueue<CAP>, C>) {
        self.step(&mut *guard);
        drop(guard);
        if !T::COMPLETION_INTERRUPT {
            self.flush();
        }
    }

    /// The one non-blocking drain step: push what the device will take now and
    /// leave the rest to the completion interrupt.
    fn step(&self, queue: &mut OutQueue<CAP>) {
        loop {
            let run: &[u8] = queue.peek();
            if run.is_empty() {
                break;
            }
            let sent = self.tx.send_ready(run);
            if sent == 0 {
                break;
            }
            queue.consume(sent);
        }
        self.arm(!queue.is_empty());
    }

    /// Keep the completion interrupt tracking the backlog exactly — armed while
    /// bytes are owed, disarmed the moment they are not, so there is neither an
    /// interrupt storm on an idle device nor a stranded backlog. A port whose
    /// transmitter cannot report completion is never asked to arm one.
    fn arm(&self, owed: bool) {
        if T::COMPLETION_INTERRUPT {
            self.tx.set_completion_interrupt(owed);
        }
    }

    /// Emit the pending loss report, if any, ahead of whatever is admitted
    /// next — which is exactly where the gap in the stream is.
    ///
    /// Attempted only with room to spare, so a queue that stays full keeps
    /// accumulating the count instead of losing reports; and a report that
    /// nonetheless fails to land charges its loss back, so no gap goes
    /// unreported.
    fn report_loss(&self, queue: &mut OutQueue<CAP>) {
        if queue.loss().is_empty() || queue.free() < LOSS_REPORT_RESERVE_BYTES {
            return;
        }
        let loss = queue.take_loss();
        let fields = [
            Field {
                key: "records",
                value: FieldValue::UnsignedInt(loss.records),
            },
            Field {
                key: "bytes",
                value: FieldValue::UnsignedInt(loss.bytes),
            },
        ];
        let event = Event {
            level: Level::Warn,
            id: CONSOLE_OUTPUT_DROPPED,
            message: "console output dropped",
            fields: &fields,
        };
        if Self::render(queue, self.tx.uptime_ms(), &event) == Admit::Refused {
            queue.restore_loss(loss);
        }
    }

    /// Render one event into the queue as a single frame.
    fn render(queue: &mut OutQueue<CAP>, uptime_ms: Option<u64>, event: &Event<'_>) -> Admit {
        queue.begin(Class::Record(event.level));
        let mut writer = FrameWriter { queue };
        write_diag_line(&mut writer, uptime_ms, true, event);
        queue.commit()
    }

    /// Bytes rendering `event` produces, so a refused record knows how much
    /// room it needs. Measured by rendering into a counter, so the answer can
    /// never disagree with the real line.
    fn measure(uptime_ms: Option<u64>, event: &Event<'_>) -> usize {
        let mut counter = ByteCounter { count: 0 };
        write_diag_line(&mut counter, uptime_ms, true, event);
        counter.count
    }

    /// Last resort when the gate cannot be acquired: render straight to the
    /// device.
    ///
    /// Reached only when a CPU died holding the gate, so the queue is
    /// unreachable and the choice is this or silence.
    fn bypass_event(&self, event: &Event<'_>) {
        let mut writer = BypassWriter { tx: &self.tx };
        write_diag_line(&mut writer, self.tx.uptime_ms(), true, event);
    }
}

/// Feed `text` to `sink` a byte at a time in the terminal line-ending
/// convention: a bare `\n` becomes `\r\n`, so a captured line renders as a
/// line break on a terminal rather than a stair-step.
///
/// The rendered line, its measured length, and its emergency copy all go
/// through this, so they can never disagree about how long a record is — a
/// disagreement would make the queue reserve the wrong amount of room. Only
/// *rendered records* are translated; a program's own output is never touched,
/// because those bytes are the program's data and not ours to reinterpret.
fn write_terminal_bytes(text: &str, mut sink: impl FnMut(u8)) {
    for byte in text.bytes() {
        if byte == b'\n' {
            sink(b'\r');
        }
        sink(byte);
    }
}

/// Appends a rendered line to the frame under construction.
struct FrameWriter<'a, const CAP: usize> {
    /// The queue whose pending frame is being written.
    queue: &'a mut OutQueue<CAP>,
}

impl<const CAP: usize> Write for FrameWriter<'_, CAP> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        write_terminal_bytes(text, |byte| self.queue.push(byte));
        Ok(())
    }
}

/// Counts the bytes a render would produce, without storing them.
struct ByteCounter {
    /// Bytes counted so far.
    count: usize,
}

impl Write for ByteCounter {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        write_terminal_bytes(text, |_| self.count += 1);
        Ok(())
    }
}

/// Writes a line straight to the device, for the died-holding-the-gate escape.
struct BypassWriter<'a, T: ConsoleTx> {
    /// The port's transmitter.
    tx: &'a T,
}

impl<T: ConsoleTx> Write for BypassWriter<'_, T> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        write_terminal_bytes(text, |byte| {
            let _ = self.tx.send_bypass(byte);
        });
        Ok(())
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
