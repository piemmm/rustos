//! Shared console-output transmit queue (`lib/conout`).
//!
//! The one architecture-neutral staging path between everything that writes to
//! a character console — the diagnostic log sink, a program's own output, an
//! interrupt handler's warning — and the device that carries it. Every kernel
//! port drives its console through this crate and supplies only its device
//! primitives ([`ConsoleTx`]) and its interrupt-masking primitives, so the
//! framing, admission policy, loss accounting, locking discipline and line
//! format exist exactly once.
//!
//! # The problem it solves
//!
//! A console transmitter carries a few thousand bytes per second while its
//! producers generate them at memory speed, run on several CPUs at once, and
//! include interrupt handlers that can fire in the middle of another producer's
//! line. Written naively, that yields three failures, all of which have been
//! observed:
//!
//! 1. **Interleaved lines.** Two CPUs writing bytes straight to the device
//!    produce one unreadable stream.
//! 2. **Spliced lines.** A queue whose drain step runs with interrupts enabled
//!    lets a handler on the same CPU contend with its own interrupted mainline,
//!    and any escape from that wait writes into the middle of the line being
//!    transmitted.
//! 3. **Silent truncation.** A byte-granular queue that drops individual bytes
//!    when full truncates a line mid-way, joins it to the next one, and reports
//!    nothing — so a capture that is missing most of its content looks
//!    complete.
//!
//! # The shape of the answer
//!
//! * [`OutQueue`] stores **whole frames**, admitted all-or-nothing, and sheds
//!   load from the tail so a line already being transmitted is never truncated.
//!   A severe record may displace queued trivia; program output is never
//!   dropped; everything shed is counted.
//! * [`ConsoleGate`] serialises every producer and every drainer behind one
//!   interrupt-masked lock, renders each record in the shared diagnostic shape,
//!   emits a [`CONSOLE_OUTPUT_DROPPED`] report at the position of any gap, and
//!   moves bytes to the device without ever waiting on it from a hot path.
//! * [`tx_wait`] gives every port the same **bounded** readiness wait, so a
//!   transmitter that is not draining costs a bounded number of polls and
//!   drops a byte, rather than hanging the kernel in an unbounded spin.
//!
//! Nothing here allocates, nothing here panics, and nothing here spins on a
//! device with interrupts masked for longer than one transmit-FIFO burst.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod events;
mod gate;
mod queue;
mod txwait;

pub use events::{CONSOLE_OUTPUT_DROPPED, CONSOLE_OUT_RANGE_END, CONSOLE_OUT_RANGE_START};
pub use gate::{ConsoleGate, ConsoleTx};
pub use queue::{
    Admit, Class, Loss, OutQueue, DEFAULT_CAPACITY_BYTES, FRAME_OVERHEAD_BYTES, MAX_RECORD_BYTES,
    MIN_CAPACITY_BYTES,
};
pub use txwait::{tx_wait, TxOutcome, TX_POLL_BUDGET};
