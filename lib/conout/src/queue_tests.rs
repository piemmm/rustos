//! Host tests for the framed console-output queue.
//!
//! They pin the guarantees the wire depends on: whole frames only, ordered
//! delivery, tail-only shedding, severity priority, never-dropped program
//! output, and loss that is always accounted for.

use super::{Admit, Class, OutQueue, FRAME_OVERHEAD_BYTES, MAX_RECORD_BYTES};
use tairix_log::Level;

/// A capacity just above the floor, so the full-queue paths are reachable with
/// a handful of frames.
const CAP: usize = 4 * (MAX_RECORD_BYTES + FRAME_OVERHEAD_BYTES);

/// A filler body size that leaves the queue genuinely unable to take another
/// one once it is full.
const FILLER: usize = 512;

/// The byte stream a device would have seen, captured without an allocator.
struct Wire {
    bytes: [u8; CAP * 2],
    len: usize,
}

impl Wire {
    const fn new() -> Self {
        Self {
            bytes: [0; CAP * 2],
            len: 0,
        }
    }

    fn extend(&mut self, run: &[u8]) {
        self.bytes[self.len..self.len + run.len()].copy_from_slice(run);
        self.len += run.len();
    }

    fn seen(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Drain the whole queue in `chunk`-byte bites — the shape a real transmitter
/// imposes, since a FIFO accepts only a few bytes at a time.
fn drain(queue: &mut OutQueue<CAP>, chunk: usize) -> Wire {
    let mut wire = Wire::new();
    loop {
        let run = queue.peek();
        if run.is_empty() {
            return wire;
        }
        let take = run.len().min(chunk);
        wire.extend(&run[..take]);
        queue.consume(take);
    }
}

/// Fill the queue with records of `level` until one is refused, returning how
/// many were accepted.
fn fill(queue: &mut OutQueue<CAP>, level: Level, body: &[u8]) -> usize {
    let mut queued = 0;
    while queue.admit(Class::Record(level), body) == Admit::Queued {
        queued += 1;
    }
    queued
}

#[test]
fn a_queued_frame_reaches_the_wire_whole_and_in_order() {
    let mut queue = OutQueue::<CAP>::new();
    assert_eq!(
        queue.admit(Class::Record(Level::Info), b"first\r\n"),
        Admit::Queued
    );
    assert_eq!(queue.admit(Class::Stream, b"second"), Admit::Queued);
    assert_eq!(queue.frames(), 2);
    assert_eq!(drain(&mut queue, CAP).seen(), b"first\r\nsecond");
    assert!(queue.is_empty());
    assert!(queue.loss().is_empty());
}

#[test]
fn only_body_bytes_reach_the_wire() {
    let mut queue = OutQueue::<CAP>::new();
    queue.admit(Class::Record(Level::Info), b"ab");
    queue.admit(Class::Record(Level::Info), b"cd");
    // A one-byte-at-a-time drain is where framing bytes would leak if the
    // cursor ever confused bookkeeping for payload.
    assert_eq!(drain(&mut queue, 1).seen(), b"abcd");
}

#[test]
fn a_refused_record_stores_nothing_and_is_counted_whole() {
    let mut queue = OutQueue::<CAP>::new();
    let body = [b'x'; FILLER];
    let queued = fill(&mut queue, Level::Debug, &body);
    assert!(queued > 0);

    let loss = queue.loss();
    assert_eq!(
        loss.records, 1,
        "the refusal that ended the fill is counted"
    );
    assert_eq!(loss.bytes, FILLER as u64);
    assert_eq!(
        queue.frames(),
        queued,
        "a refusal leaves the queue untouched"
    );

    let wire = drain(&mut queue, 7);
    assert_eq!(wire.seen().len(), queued * FILLER);
    assert!(wire.seen().chunks(FILLER).all(|line| line == body));
}

#[test]
fn a_record_longer_than_the_frame_bound_is_refused_not_truncated() {
    let mut queue = OutQueue::<CAP>::new();
    let overlong = [b'y'; MAX_RECORD_BYTES + 1];
    assert_eq!(
        queue.admit(Class::Record(Level::Info), &overlong),
        Admit::Refused
    );
    assert!(queue.is_empty());
    assert_eq!(queue.loss().records, 1);
    assert_eq!(drain(&mut queue, CAP).seen(), b"");
}

#[test]
fn a_severe_record_evicts_the_newest_lower_severity_record() {
    let mut queue = OutQueue::<CAP>::new();
    let body = [b'd'; FILLER];
    let queued = fill(&mut queue, Level::Debug, &body);
    queue.take_loss();

    // Same size as the body the fill could not place, so the queue is provably
    // short of room for it.
    let alarm = [b'!'; FILLER];
    assert!(
        queue.shortfall(alarm.len()) > 0,
        "the queue is genuinely full"
    );
    assert!(queue.evict_tail_below(Level::Critical, alarm.len()));
    assert_eq!(
        queue.admit(Class::Record(Level::Critical), &alarm),
        Admit::Queued
    );

    let loss = queue.loss();
    assert_eq!(loss.records, 1, "exactly one debug record made room");
    assert_eq!(loss.bytes, FILLER as u64);

    let wire = drain(&mut queue, 13);
    assert_eq!(wire.seen().len(), queued * FILLER);
    assert!(
        wire.seen().ends_with(&alarm),
        "the severe record is last, in arrival order"
    );
}

#[test]
fn eviction_stops_at_an_equally_severe_record() {
    let mut queue = OutQueue::<CAP>::new();
    let body = [b'w'; FILLER];
    fill(&mut queue, Level::Warn, &body);
    queue.take_loss();

    assert!(!queue.evict_tail_below(Level::Warn, FILLER));
    assert!(
        queue.loss().is_empty(),
        "a refused eviction charges nothing"
    );
}

#[test]
fn program_output_is_never_evicted_by_any_record() {
    let mut queue = OutQueue::<CAP>::new();
    let body = [b's'; FILLER];
    while queue.admit(Class::Stream, &body) == Admit::Queued {}
    queue.take_loss();

    assert!(!queue.evict_tail_below(Level::Critical, FILLER));
    let wire = drain(&mut queue, 64);
    assert!(wire.seen().chunks(FILLER).all(|chunk| chunk == body));
}

#[test]
fn the_partially_transmitted_head_is_never_evicted() {
    let mut queue = OutQueue::<CAP>::new();
    let body = [b'h'; MAX_RECORD_BYTES];
    assert_eq!(
        queue.admit(Class::Record(Level::Trace), &body),
        Admit::Queued
    );
    queue.consume(4);
    fill(&mut queue, Level::Trace, &body);
    queue.take_loss();

    // Demand more room than the queue can ever free, so eviction walks all the
    // way down to the frame whose first bytes are already on the wire.
    assert!(!queue.evict_tail_below(Level::Critical, CAP));
    assert_eq!(queue.frames(), 1, "only the in-flight head survives");
    assert_eq!(
        drain(&mut queue, MAX_RECORD_BYTES).seen().len(),
        MAX_RECORD_BYTES - 4,
        "the head resumes exactly where the device left off"
    );
}

#[test]
fn program_output_reports_an_honest_short_write_when_full() {
    let mut queue = OutQueue::<CAP>::new();
    let filler = [b'f'; FILLER];
    fill(&mut queue, Level::Debug, &filler);
    queue.take_loss();

    let payload = [b'p'; FILLER];
    let accepted = queue.admit_prefix(Class::Stream, &payload);
    assert!(accepted < payload.len(), "a full queue cannot take it all");
    assert!(queue.loss().is_empty(), "a short write is not a loss");

    let wire = drain(&mut queue, 64);
    let seen = wire.seen();
    assert_eq!(
        &seen[seen.len() - accepted..],
        &payload[..accepted],
        "exactly the accepted prefix reached the wire"
    );
}

#[test]
fn a_short_write_of_zero_is_reported_rather_than_swallowed() {
    let mut queue = OutQueue::<CAP>::new();
    let body = [b'z'; MAX_RECORD_BYTES];
    while queue.admit(Class::Stream, &body) == Admit::Queued {}
    queue.take_loss();
    assert_eq!(queue.admit_prefix(Class::Stream, b"more"), 0);
    assert!(queue.loss().is_empty());
}

#[test]
fn a_rendered_record_that_overflows_is_rolled_back_and_retryable() {
    let mut queue = OutQueue::<CAP>::new();
    let filler = [b'r'; FILLER];
    fill(&mut queue, Level::Debug, &filler);
    queue.take_loss();

    let line = [b'l'; FILLER];
    queue.begin(Class::Record(Level::Error));
    for &byte in &line {
        queue.push(byte);
    }
    assert_eq!(queue.commit(), Admit::Refused);
    assert!(queue.shortfall(line.len()) > 0);
    assert_eq!(
        queue.loss().records,
        1,
        "the refused render is charged once"
    );

    assert!(queue.evict_tail_below(Level::Error, line.len()));
    assert_eq!(
        queue.admit(Class::Record(Level::Error), &line),
        Admit::Queued
    );
    let wire = drain(&mut queue, 32);
    assert!(
        wire.seen().ends_with(&line),
        "the retried render lands whole"
    );
}

#[test]
fn taken_loss_can_be_charged_back_when_its_report_cannot_be_queued() {
    let mut queue = OutQueue::<CAP>::new();
    queue.admit(Class::Record(Level::Info), &[b'q'; MAX_RECORD_BYTES + 1]);
    let taken = queue.take_loss();
    assert_eq!(taken.records, 1);
    assert!(queue.loss().is_empty());
    queue.restore_loss(taken);
    assert_eq!(queue.loss(), taken, "no loss is ever forgotten");
}

#[test]
fn frames_wrap_the_buffer_without_corrupting_the_stream() {
    let mut queue = OutQueue::<CAP>::new();
    // Odd sizes, many times the capacity, so headers, footers and bodies all
    // straddle the wrap repeatedly.
    for round in 0..64u8 {
        let body = [round; 397];
        assert_eq!(
            queue.admit(Class::Record(Level::Info), &body),
            Admit::Queued
        );
        assert_eq!(
            queue.admit(Class::Stream, &[round.wrapping_add(1); 101]),
            Admit::Queued
        );
        let wire = drain(&mut queue, 37);
        assert_eq!(&wire.seen()[..397], &body[..]);
        assert_eq!(wire.seen().len(), 397 + 101);
    }
    assert!(queue.is_empty());
    assert!(queue.loss().is_empty());
}

#[test]
fn transmitted_bytes_are_scrubbed_from_the_queue_storage() {
    let mut queue = OutQueue::<CAP>::new();
    let secret = b"passphrase-echo";
    queue.admit(Class::Stream, secret);
    assert_eq!(drain(&mut queue, CAP).seen(), secret);
    assert!(
        !queue
            .storage()
            .windows(secret.len())
            .any(|window| window == secret),
        "delivered program output does not linger in kernel memory"
    );
}

#[test]
fn a_refused_render_is_scrubbed_from_the_queue_storage() {
    let mut queue = OutQueue::<CAP>::new();
    let filler = [b'k'; FILLER];
    fill(&mut queue, Level::Debug, &filler);
    let secret = b"refused-secret";
    queue.begin(Class::Stream);
    for _ in 0..=(MAX_RECORD_BYTES + 1) / secret.len() {
        for &byte in secret {
            queue.push(byte);
        }
    }
    assert_eq!(queue.commit(), Admit::Refused);
    assert!(
        !queue
            .storage()
            .windows(secret.len())
            .any(|window| window == secret),
        "a rolled-back frame leaves nothing behind"
    );
}

#[test]
fn an_interrupted_render_leaves_no_half_frame_behind() {
    let mut queue = OutQueue::<CAP>::new();
    queue.begin(Class::Record(Level::Info));
    for &byte in b"abandoned" {
        queue.push(byte);
    }
    // A second `begin` is what a re-entrant producer would do; the abandoned
    // body must not reach the wire.
    queue.begin(Class::Record(Level::Info));
    for &byte in b"complete\r\n" {
        queue.push(byte);
    }
    assert_eq!(queue.commit(), Admit::Queued);
    assert_eq!(drain(&mut queue, CAP).seen(), b"complete\r\n");
    assert!(queue.loss().is_empty());
}

#[test]
fn an_empty_body_owes_the_device_nothing() {
    let mut queue = OutQueue::<CAP>::new();
    assert_eq!(queue.admit(Class::Stream, b""), Admit::Queued);
    assert!(
        queue.is_empty(),
        "an empty admission leaves no frame the device can never drain"
    );
    assert!(queue.loss().is_empty());
    // The queue is still usable, and the empty admission left no gap in it.
    assert_eq!(
        queue.admit(Class::Record(Level::Info), b"real\r\n"),
        Admit::Queued
    );
    assert_eq!(drain(&mut queue, CAP).seen(), b"real\r\n");
}

#[test]
fn a_push_with_no_open_frame_is_ignored() {
    let mut queue = OutQueue::<CAP>::new();
    queue.push(b'!');
    assert!(queue.is_empty());
    assert_eq!(queue.commit(), Admit::Refused);
    assert!(queue.loss().is_empty(), "a misuse charges no phantom loss");
    assert_eq!(drain(&mut queue, CAP).seen(), b"");
}
