//! Host tests for the console-output gate.
//!
//! The device is simulated so the tests can stall it, unstall it, and wedge it
//! on demand — the three conditions under which a real console corrupts or
//! silently loses output.
//!
//! What a host test cannot simulate is a genuine interrupt, so the guarantee
//! that every hold masks the current CPU's interrupts is structural: the gate
//! holds its queue through [`IrqSafeSpinLock`] parameterised by the port's own
//! interrupt control, and each port pins its own choice of control at compile
//! time. Here the control is the no-op one, as it must be on a host.

use core::cell::{Cell, RefCell};

use tairix_log::{field::FieldValue, Event, EventId, Field, Level};
use tairix_sync::irq::NopInterruptControl;

use super::{ByteCounter, ConsoleGate, ConsoleTx};
use crate::queue::{MAX_RECORD_BYTES, MIN_CAPACITY_BYTES};

/// Queue capacity for the gate under test: the floor, so a full queue is
/// reachable with a modest number of records.
const CAP: usize = MIN_CAPACITY_BYTES;

/// The uptime the simulated transmitter reports.
///
/// A test that compares a measured line length against the transmitted one
/// must measure at this stamp, because the gate renders at whatever its own
/// transmitter reports — the stamp is part of the line.
const UPTIME_MS: u64 = 1_234;

/// A generous recording buffer: several queue-fulls of transmitted bytes.
const WIRE_CAP: usize = CAP * 3;

/// Bytes the simulated device has transmitted, in order.
struct Wire {
    /// Recorded bytes.
    bytes: [u8; WIRE_CAP],
    /// Bytes recorded.
    len: usize,
}

impl Wire {
    /// An empty recording.
    const fn new() -> Self {
        Self {
            bytes: [0; WIRE_CAP],
            len: 0,
        }
    }

    /// Record `run`.
    ///
    /// Overflowing is a failure rather than a truncation: a test that
    /// transmits more than this records would otherwise assert against a
    /// silently cut stream and read as passing.
    fn record(&mut self, run: &[u8]) {
        assert!(
            self.len + run.len() <= WIRE_CAP,
            "the recording buffer is too small for this test"
        );
        self.bytes[self.len..self.len + run.len()].copy_from_slice(run);
        self.len += run.len();
    }
}

/// A simulated console transmitter.
///
/// `DEFER` selects the two drain policies a real port can have: a transmitter
/// that raises a completion interrupt (`true`), and one that cannot, so the
/// producer must finish the transmission itself (`false`).
struct TestTx<const DEFER: bool> {
    /// Bytes transmitted, in order.
    wire: RefCell<Wire>,
    /// Bytes a non-blocking send will still accept before the simulated FIFO is
    /// full.
    window: Cell<usize>,
    /// Whether a bounded-waiting send refuses everything.
    wedged: Cell<bool>,
    /// Whether the completion interrupt is currently armed.
    armed: Cell<bool>,
    /// Bytes written straight to the device, bypassing the queue.
    bypassed: Cell<usize>,
}

impl<const DEFER: bool> TestTx<DEFER> {
    /// A transmitter whose FIFO accepts nothing until [`Self::open`] is called.
    const fn stalled() -> Self {
        Self {
            wire: RefCell::new(Wire::new()),
            window: Cell::new(0),
            wedged: Cell::new(false),
            armed: Cell::new(false),
            bypassed: Cell::new(0),
        }
    }

    /// Let the device accept `bytes` more.
    fn open(&self, bytes: usize) {
        self.window.set(bytes);
    }

    /// Take everything transmitted so far, as an owned copy.
    fn seen(&self) -> ([u8; WIRE_CAP], usize) {
        let wire = self.wire.borrow();
        (wire.bytes, wire.len)
    }
}

impl<const DEFER: bool> ConsoleTx for TestTx<DEFER> {
    const COMPLETION_INTERRUPT: bool = DEFER;

    fn uptime_ms(&self) -> Option<u64> {
        Some(UPTIME_MS)
    }

    fn send_ready(&self, bytes: &[u8]) -> usize {
        let take = bytes.len().min(self.window.get());
        if take != 0 {
            self.window.set(self.window.get() - take);
            self.wire.borrow_mut().record(&bytes[..take]);
        }
        take
    }

    fn send_bounded(&self, bytes: &[u8]) -> usize {
        if self.wedged.get() {
            return 0;
        }
        self.wire.borrow_mut().record(bytes);
        bytes.len()
    }

    fn set_completion_interrupt(&self, armed: bool) {
        assert!(
            DEFER,
            "a port without a completion interrupt never arms one"
        );
        self.armed.set(armed);
    }

    fn send_bypass(&self, byte: u8) -> bool {
        self.bypassed.set(self.bypassed.get() + 1);
        self.wire.borrow_mut().record(&[byte]);
        true
    }
}

/// A gate over a transmitter that defers its backlog to a completion interrupt.
type DeferredGate = ConsoleGate<TestTx<true>, NopInterruptControl, CAP>;

/// A gate over a transmitter that cannot report completion, so producers finish
/// the transmission themselves.
type WriteThroughGate = ConsoleGate<TestTx<false>, NopInterruptControl, CAP>;

/// One test record.
fn record(level: Level, id: u32, message: &str) -> Event<'_> {
    Event {
        level,
        id: EventId(id),
        message,
        fields: &[],
    }
}

/// Whether `haystack` contains `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|slot| slot == needle)
}

/// Where `needle` first occurs in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|slot| slot == needle)
}

/// Complete lines the device saw, counted by their terminator.
fn line_count(wire: &[u8]) -> usize {
    wire.windows(2).filter(|pair| *pair == b"\r\n").count()
}

/// How many times `needle` occurs in `wire`.
fn count_byte(wire: &[u8], needle: u8) -> usize {
    wire.iter()
        .fold(0, |seen, byte| seen + usize::from(*byte == needle))
}

/// The whole line `needle` occurs on, without its terminator.
///
/// Needed because the parts of a record are spread across its line: the level
/// tag precedes the `id=` token, so a check anchored on the id would miss it.
fn line_with<'a>(wire: &'a [u8], needle: &[u8]) -> Option<&'a [u8]> {
    let at = find(wire, needle)?;
    let start = wire[..at]
        .windows(2)
        .rposition(|pair| pair == b"\r\n")
        .map_or(0, |break_at| break_at + 2);
    let end = find(&wire[at..], b"\r\n").map_or(wire.len(), |offset| at + offset);
    Some(&wire[start..end])
}

/// A filler message long enough that a few hundred of them overrun the queue.
const FILLER: &str = "filler payload that makes each record substantial enough \
     to fill a modest queue in a couple of hundred records";

#[test]
fn a_record_stalled_behind_a_full_device_reaches_the_wire_whole_and_in_order() {
    let gate = DeferredGate::new(TestTx::stalled());
    gate.write_event(&record(Level::Info, 4_001, "first line"));
    gate.write_event(&record(Level::Info, 4_002, "second line"));
    let (_, len) = gate.tx().seen();
    assert_eq!(len, 0, "a stalled device transmits nothing");

    gate.tx().open(usize::MAX);
    gate.pump();

    let (wire, len) = gate.tx().seen();
    let wire = &wire[..len];
    assert_eq!(line_count(wire), 2, "both lines completed");
    let first = find(wire, b"id=4001").expect("first record");
    let second = find(wire, b"id=4002").expect("second record");
    assert!(first < second, "delivery preserves admission order");
}

#[test]
fn a_line_break_is_carriage_return_and_newline_on_the_wire() {
    let gate = DeferredGate::new(TestTx::stalled());
    gate.tx().open(usize::MAX);
    gate.write_event(&record(Level::Info, 4_003, "one line"));
    let (wire, len) = gate.tx().seen();
    let wire = &wire[..len];
    assert!(
        wire.ends_with(b"\r\n"),
        "a capture needs the carriage return"
    );
    assert_eq!(line_count(wire), 1);
}

#[test]
fn dropped_output_is_reported_on_the_wire_where_the_gap_is() {
    let gate = DeferredGate::new(TestTx::stalled());
    // Fill the queue with a stalled device, so records start being refused.
    for index in 0..200u32 {
        gate.write_event(&record(Level::Debug, 4_100 + index, FILLER));
    }

    // Let it drain, then admit one more record: the report belongs between what
    // survived and what comes after the gap.
    gate.tx().open(usize::MAX);
    gate.pump();
    gate.write_event(&record(Level::Info, 4_999, "after the gap"));
    gate.pump();

    let (wire, len) = gate.tx().seen();
    let wire = &wire[..len];
    let report = find(wire, b"id=18001").expect("the loss is reported, never silent");
    let after = find(wire, b"id=4999").expect("the record admitted after the gap");
    assert!(report < after, "the report marks where output was lost");

    let line = line_with(wire, b"id=18001").expect("the report is a whole line");
    assert!(
        contains(line, b"records="),
        "it says how many lines were lost"
    );
    assert!(contains(line, b"bytes="), "and how much payload");
    assert!(
        contains(line, b"WARN"),
        "lost output is a warning, not a quiet note"
    );
}

#[test]
fn the_loss_report_is_emitted_once_per_gap_not_once_per_record() {
    let gate = DeferredGate::new(TestTx::stalled());
    for index in 0..200u32 {
        gate.write_event(&record(Level::Debug, 4_200 + index, FILLER));
    }
    gate.tx().open(usize::MAX);
    gate.pump();
    for index in 0..20u32 {
        gate.write_event(&record(Level::Info, 4_500 + index, "after"));
        gate.pump();
    }

    let (wire, len) = gate.tx().seen();
    let reports = wire[..len]
        .windows(8)
        .filter(|slot| *slot == b"id=18001")
        .count();
    assert_eq!(reports, 1, "one gap, one report");
}

#[test]
fn a_severe_record_displaces_queued_trivia_rather_than_being_lost() {
    let gate = DeferredGate::new(TestTx::stalled());
    for index in 0..200u32 {
        gate.write_event(&record(Level::Trace, 4_300 + index, FILLER));
    }
    gate.write_event(&record(Level::Critical, 4_777, "the machine is on fire"));

    gate.tx().open(usize::MAX);
    gate.pump();

    let (wire, len) = gate.tx().seen();
    assert!(
        contains(&wire[..len], b"id=4777"),
        "a critical record survives a flood of trace output"
    );
}

#[test]
fn program_output_is_never_displaced_by_a_record() {
    let gate = DeferredGate::new(TestTx::stalled());
    let payload = [b'o'; 512];
    let mut written = 0;
    while written < CAP {
        let accepted = gate.write_output(&payload);
        if accepted == 0 {
            break;
        }
        written += accepted;
    }
    gate.write_event(&record(Level::Critical, 4_888, "would displace a record"));

    gate.tx().open(usize::MAX);
    gate.pump();

    let (wire, len) = gate.tx().seen();
    let transmitted = count_byte(&wire[..len], b'o');
    assert_eq!(
        transmitted, written,
        "every accepted byte of program output was delivered"
    );
}

#[test]
fn program_output_beyond_the_queue_waits_for_the_device_rather_than_stalling() {
    // The device takes nothing through the non-blocking path, so the queue
    // fills; its bounded path drains. More output than the queue can hold must
    // still be accepted in full, because a zero-length write would tell the
    // writing program its output failed while the console is working.
    let gate = DeferredGate::new(TestTx::stalled());
    let payload = [b'p'; MAX_RECORD_BYTES];
    let rounds = 6;
    for _ in 0..rounds {
        assert_eq!(
            gate.write_output(&payload),
            payload.len(),
            "a working console never reports a stalled write"
        );
    }

    gate.tx().open(usize::MAX);
    gate.flush();
    let (wire, len) = gate.tx().seen();
    assert_eq!(
        count_byte(&wire[..len], b'p'),
        rounds * payload.len(),
        "every accepted byte reached the device"
    );
    assert_eq!(
        len,
        rounds * payload.len(),
        "nothing was lost, so no loss report interrupts the output"
    );
}

#[test]
fn a_wedged_device_costs_queued_output_but_never_wedges_the_writer() {
    let gate = DeferredGate::new(TestTx::stalled());
    gate.tx().wedged.set(true);
    let payload = [b'w'; MAX_RECORD_BYTES];
    for _ in 0..6 {
        // The shape of a caller draining its own buffer: each call takes a
        // prefix, and none of them may report a stall, or the loop would give
        // up on output the console could still carry.
        let mut written = 0;
        while written < payload.len() {
            let accepted = gate.write_output(&payload[written..]);
            assert_ne!(
                accepted, 0,
                "the writer makes progress even against a dead transmitter"
            );
            written += accepted;
        }
    }

    // Recovered: the operator learns output was lost, and where.
    gate.tx().wedged.set(false);
    gate.tx().open(usize::MAX);
    gate.write_event(&record(Level::Info, 4_009, "after the wedge"));
    gate.flush();
    let (wire, len) = gate.tx().seen();
    let wire = &wire[..len];
    let line = line_with(wire, b"id=18001").expect("the dropped output is reported");
    assert!(contains(line, b"records="));
    assert!(
        find(wire, b"id=18001") < find(wire, b"id=4009"),
        "the report precedes what was admitted after the gap"
    );
}

#[test]
fn a_deferred_port_arms_the_completion_interrupt_only_while_output_is_owed() {
    let gate = DeferredGate::new(TestTx::stalled());
    gate.write_event(&record(Level::Info, 4_004, "owed to the device"));
    assert!(
        gate.tx().armed.get(),
        "output is waiting, so the completion interrupt must be armed"
    );

    gate.tx().open(usize::MAX);
    gate.service_completion();
    assert!(
        !gate.tx().armed.get(),
        "an empty queue disarms, so an idle device raises no interrupt storm"
    );
}

#[test]
fn a_write_through_port_delivers_before_the_producer_returns() {
    let gate = WriteThroughGate::new(TestTx::stalled());
    gate.write_event(&record(Level::Info, 4_005, "no completion interrupt here"));
    let (wire, len) = gate.tx().seen();
    assert!(
        contains(&wire[..len], b"id=4005"),
        "with nothing to finish the transmission later, the producer finishes it"
    );
}

#[test]
fn a_wedged_transmitter_abandons_whole_lines_and_counts_them() {
    let gate = WriteThroughGate::new(TestTx::stalled());
    gate.tx().wedged.set(true);
    gate.write_event(&record(Level::Info, 4_006, "into the void"));
    let (_, len) = gate.tx().seen();
    assert_eq!(len, 0, "a wedged transmitter takes nothing");

    // Recovered: the next line must be whole, and the loss reported.
    gate.tx().wedged.set(false);
    gate.write_event(&record(Level::Info, 4_007, "after recovery"));
    let (wire, len) = gate.tx().seen();
    let wire = &wire[..len];
    let report = find(wire, b"id=18001").expect("the abandoned line is reported");
    let after = find(wire, b"id=4007").expect("the line after recovery");
    assert!(report < after);
    assert!(
        !contains(wire, b"id=4006"),
        "the abandoned line is not resumed out of context"
    );
    assert_eq!(line_count(wire), 2, "only whole lines reach the wire");
}

#[test]
fn a_contended_drain_step_is_a_no_op_rather_than_a_wait() {
    let gate = DeferredGate::new(TestTx::stalled());
    gate.tx().open(usize::MAX);
    // Nothing queued: the step must neither transmit nor arm anything.
    gate.pump();
    let (_, len) = gate.tx().seen();
    assert_eq!(len, 0);
    assert!(!gate.tx().armed.get());
}

#[test]
fn the_measured_length_matches_what_the_render_actually_produces() {
    let fields = [
        Field {
            key: "records",
            value: FieldValue::UnsignedInt(u64::MAX),
        },
        Field {
            key: "bytes",
            value: FieldValue::UnsignedInt(u64::MAX),
        },
    ];
    let event = Event {
        level: Level::Warn,
        id: EventId(18_001),
        message: "console output dropped",
        fields: &fields,
    };
    let measured = DeferredGate::measure(Some(UPTIME_MS), &event);

    let gate = DeferredGate::new(TestTx::stalled());
    gate.tx().open(usize::MAX);
    gate.write_event(&event);
    let (_, len) = gate.tx().seen();
    assert_eq!(
        measured, len,
        "an eviction sized from the measurement must free exactly enough"
    );
}

#[test]
fn the_longest_possible_loss_report_fits_the_room_reserved_for_it() {
    let fields = [
        Field {
            key: "records",
            value: FieldValue::UnsignedInt(u64::MAX),
        },
        Field {
            key: "bytes",
            value: FieldValue::UnsignedInt(u64::MAX),
        },
    ];
    let event = Event {
        level: Level::Warn,
        id: super::CONSOLE_OUTPUT_DROPPED,
        message: "console output dropped",
        fields: &fields,
    };
    let mut counter = ByteCounter { count: 0 };
    tairix_log::write_diag_line(&mut counter, Some(u64::MAX), true, &event);
    assert!(
        counter.count <= super::LOSS_REPORT_RESERVE_BYTES,
        "the reserve must cover the worst case, or a report could be lost"
    );
}
