//! Shared tty line discipline (`lib/tty`).
//!
//! A terminal is not a raw byte pipe: between the program and the human sits
//! a *line discipline* that cooks bytes in both directions — it echoes what
//! the user types, turns the Return key and a program's bare line feeds into
//! a carriage-return/line-feed pair so the cursor both drops a row and
//! returns to column zero, rubs out the previous character on Backspace,
//! turns `Ctrl-C`/`Ctrl-Z` into job-control signals for the foreground job,
//! and bounds a read at the end of a line so type-ahead survives the reader
//! that is only entitled to the line in front of it. TAIRiX has exactly
//! **one** definition of that discipline, and this crate is it.
//!
//! Two consumers drive the same code: the kernel console device
//! (`kernel/core::console`) that a hardware-console-backed shell reads and
//! writes, and the pseudo-terminal (`plans/PTY.md`) whose slave gives the
//! graphical terminal's shell the identical console-like behaviour. Neither
//! carries a private copy of the cooking rules.
//!
//! # Sink-agnostic
//!
//! The discipline owns no I/O. Each entry point takes the caller's byte sink
//! as a closure, so the same logic drives a `ConsoleWrite` device, a
//! pty ring buffer, or a test recorder without knowing which:
//!
//! - [`write_cooked`] applies the output (`ONLCR`) translation through a
//!   *fallible* sink (it preserves the POSIX short-write contract, so the
//!   caller's `stream_write` can loop).
//! - [`EchoLine::echo`] applies the input local-echo through a *best-effort*
//!   sink (echo is cosmetic, so a short write or device error is swallowed
//!   rather than failing the read the user asked for).
//! - [`read_bounded`] applies the input read bound through the caller's
//!   *queue* as a closure, so the console's type-ahead ring and the pty's
//!   input ring share one rule for where a read stops.
//! - [`job_control_signal`] and [`is_line_delimiter`] are pure classification
//!   with no sink at all.
//!
//! # Assembled, not re-implemented
//!
//! The control-byte constants and the Delete-key escape recogniser
//! ([`tairix_vt::EraseSeq`]) are `lib/vt`'s single definition; this
//! crate is the *assembly* of them into a discipline, never a second copy of
//! the vocabulary.
//!
//! Fail-closed and total: there is no `unwrap` / `expect` / `panic!` here, and
//! nothing writes to fd 3 (`stdinfo` is reserved).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(test)]
extern crate alloc;

use tairix_abi::{Errno, Signal};
use tairix_vt::control;
use tairix_vt::line::EraseSeq;

/// The `^C` interrupt byte (`ETX`) the cooked line discipline maps to
/// [`Signal::Interrupt`] while a foreground job is set.
pub const INTERRUPT_BYTE: u8 = 0x03;

/// The `^Z` stop byte (`SUB`) the cooked line discipline maps to
/// [`Signal::Stop`] while a foreground job is set.
pub const STOP_BYTE: u8 = 0x1A;

/// The largest printable run [`EchoLine::echo`] batches into one sink call
/// before flushing. Purely a device-round-trip optimisation; the discipline
/// is correct for any positive value (a control byte flushes early anyway).
const ECHO_RUN: usize = 64;

/// Whether `byte` ends a line of terminal input: the carriage return a
/// terminal sends for the Return key, or the line feed a piped or pty writer
/// sends for it (the shared [`tairix_vt::control`] vocabulary, so "what ends a
/// line" has one definition on the echo path, the secret-marker path, and the
/// read bound below).
#[must_use]
pub const fn is_line_delimiter(byte: u8) -> bool {
    matches!(byte, control::CR | control::LF)
}

/// Take **at most one line** of terminal input from `next` into `out`, and
/// report how many bytes were taken.
///
/// This is the terminal read bound: bytes are taken until `out` is full,
/// `next` is exhausted, or a line delimiter has been taken — the delimiter is
/// included, and everything queued behind it is left where it was.
///
/// # Why a terminal read stops at the line boundary
///
/// A terminal's queued input belongs to the *terminal*, not to whichever
/// process happens to read first. A reader that took bytes past the line it
/// was asked for would own them privately, and a reader that then hands the
/// terminal on — a login that authenticates and launches the session shell, a
/// shell that runs a foreground child — takes those bytes with it and the
/// keystrokes are gone: what the user typed ahead was accepted, echoed, and
/// then silently lost. Stopping at the delimiter makes that unrepresentable
/// for every reader, including one whose code we do not control, rather than
/// trusting each program to ask for no more than it will consume.
///
/// Every terminal reader already handles a short read — input arrives one
/// keystroke at a time — so a bound can only shorten a read a caller loops on,
/// never change what it eventually sees. No key's escape sequence carries a
/// delimiter, so a bound never splits one.
///
/// `next` yields the queue's next byte and removes it, so a byte it hands over
/// is always placed in `out`: nothing is taken that is not delivered.
#[must_use]
pub fn read_bounded<F>(out: &mut [u8], mut next: F) -> usize
where
    F: FnMut() -> Option<u8>,
{
    let mut taken = 0usize;
    for slot in out.iter_mut() {
        let Some(byte) = next() else {
            break;
        };
        *slot = byte;
        taken += 1;
        if is_line_delimiter(byte) {
            break;
        }
    }
    taken
}

/// Classify one input byte as a cooked-mode job-control signal, if it is one.
///
/// Returns [`Signal::Interrupt`] for `^C` ([`INTERRUPT_BYTE`]) and
/// [`Signal::Stop`] for `^Z` ([`STOP_BYTE`]); [`None`] for every other byte,
/// which the caller forwards to the input buffer unchanged. This is the pure
/// recognition half of the cooked-mode interception: the caller owns the
/// policy (only intercept in cooked mode with a foreground job installed) and
/// the delivery (queue the signal for the foreground task).
#[must_use]
pub const fn job_control_signal(byte: u8) -> Option<Signal> {
    match byte {
        INTERRUPT_BYTE => Some(Signal::Interrupt),
        STOP_BYTE => Some(Signal::Stop),
        _ => None,
    }
}

/// Write program-output `bytes` through `write`, cooking output line feeds:
/// a bare line feed (`LF`) is emitted as the `CR LF` pair (the `ONLCR`
/// output translation an interactive terminal applies) so the cursor returns
/// to column zero as it advances a line, instead of dropping a line beneath
/// the current column — the "staircase" a raw `LF` produces on a terminal
/// whose line feed is a pure line feed. A carriage return passes through
/// unchanged.
///
/// `write` is the caller's fallible byte sink (`Ok(n)` wrote `n` of the
/// offered bytes, `Ok(0)` accepted none, `Err` failed); it may short-write
/// and is called repeatedly.
///
/// Returns the number of **input** bytes consumed, which is not the count
/// written to the sink when a line feed expanded to two bytes: a short sink
/// write maps back to a short write the caller loops on, preserving the
/// POSIX short-write contract. A sink error before any byte is consumed
/// surfaces as `Err`; once some input has been consumed, a later stall
/// reports the partial count so no byte is lost or double-written on retry.
///
/// # Errors
///
/// Returns the sink's [`Errno`] when it rejects the first byte.
pub fn write_cooked<F>(bytes: &[u8], mut write: F) -> Result<usize, Errno>
where
    F: FnMut(&[u8]) -> Result<usize, Errno>,
{
    let mut consumed = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let run_start = index;
        while index < bytes.len() && bytes[index] != control::LF {
            index += 1;
        }
        if index > run_start {
            let run = &bytes[run_start..index];
            match write(run) {
                Ok(0) => return Ok(consumed),
                Ok(written) => {
                    let written = written.min(run.len());
                    consumed += written;
                    if written < run.len() {
                        return Ok(consumed);
                    }
                }
                Err(err) if consumed == 0 => return Err(err),
                Err(_) => return Ok(consumed),
            }
        }
        if index < bytes.len() {
            match write(b"\r\n") {
                Ok(0) => return Ok(consumed),
                Ok(written) if written >= 2 => {
                    consumed += 1;
                    index += 1;
                }
                Ok(written) => {
                    if write_all(&mut write, &b"\r\n"[written.min(2)..]) {
                        consumed += 1;
                        index += 1;
                    } else {
                        return Ok(consumed);
                    }
                }
                Err(err) if consumed == 0 => return Err(err),
                Err(_) => return Ok(consumed),
            }
        }
    }
    Ok(consumed)
}

/// Write every byte of `bytes` through `write`, looping over short writes and
/// stopping on a closed/erroring sink (never spin). Returns whether every
/// byte reached the sink.
fn write_all<F>(write: &mut F, mut bytes: &[u8]) -> bool
where
    F: FnMut(&[u8]) -> Result<usize, Errno>,
{
    while !bytes.is_empty() {
        match write(bytes) {
            Ok(0) | Err(_) => return false,
            Ok(n) => bytes = &bytes[n.min(bytes.len())..],
        }
    }
    true
}

/// The input local-echo half of the line discipline: the per-line editing
/// state one edited input line needs, carried across the many reads a line
/// spans.
///
/// A reader drains its input a byte (or a few) at a time, so one logical
/// input line — and one split Delete escape sequence — spans many
/// [`echo`](Self::echo) calls; the rub-out bound (`col`) and the held Delete
/// prefix (`seq`) are therefore state, not recomputed per call.
///
/// `col` is the column of the line-discipline cursor since the last line
/// terminator (or [`reset`](Self::reset)): the count of characters the user
/// has typed and the echo has rendered on the current input line. It bounds
/// the erase (rub-out) — an erase rubs out one rendered character only while
/// this is non-zero, so a Backspace at the start of the input line never
/// walks the cursor back into the prompt the program wrote.
#[derive(Debug, Default)]
pub struct EchoLine {
    col: usize,
    seq: EraseSeq,
}

impl EchoLine {
    /// A fresh echo state at column zero holding no Delete prefix.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            col: 0,
            seq: EraseSeq::new(),
        }
    }

    /// Reset the editing state to the start of a fresh line: column zero, no
    /// held Delete prefix.
    ///
    /// The caller invokes this when the read line discipline restarts a line
    /// out of band — a change of input mode (a secret password read and the
    /// prompt that follows it each start a fresh edited line), so a later
    /// Backspace must not rub out into a line the column was last counting
    /// before the change.
    pub fn reset(&mut self) {
        self.col = 0;
        self.seq = EraseSeq::new();
    }

    /// Echo `bytes` (the bytes a read just consumed) back to the terminal
    /// output through the best-effort sink `emit`, so an interactive user
    /// sees what they type (terminal local echo).
    ///
    /// A carriage return or line feed is echoed as the `CR LF` pair so the
    /// cursor both returns to column zero *and* advances a line — a bare `CR`
    /// (what a serial terminal sends for the Return key) would otherwise
    /// overwrite the current line. An **erase** (rub-out) — the single-byte
    /// Backspace/Delete ([`tairix_vt::control::is_line_erase`]) or the Delete key's
    /// `CSI 3 ~` escape sequence (the shared [`tairix_vt::EraseSeq`] recogniser, so a
    /// Delete keypress never paints raw escape glyphs) — is *not* echoed
    /// verbatim; instead it rubs out the previous character with the
    /// `BS SP BS` [`tairix_vt::control::ERASE_ECHO`] sequence, but only while a
    /// character on the current input line remains to erase (`col`). An erase
    /// at the start of the line is a no-op, so it never walks the cursor back
    /// over the prompt.
    ///
    /// `emit` is best-effort: echo is purely cosmetic, so the caller swallows
    /// a short write or sink error rather than failing the read the user
    /// asked for. Printable bytes are batched into bounded runs before an
    /// `emit` call to keep device round-trips down; a control byte flushes the
    /// pending run first.
    pub fn echo<F>(&mut self, bytes: &[u8], mut emit: F)
    where
        F: FnMut(&[u8]),
    {
        let mut run = [0u8; ECHO_RUN];
        let mut run_len = 0usize;
        for &byte in bytes {
            let step = self.seq.feed(byte);
            if step.erase() {
                flush(&mut emit, &run, &mut run_len);
                if self.col > 0 {
                    emit(&control::ERASE_ECHO);
                    self.col -= 1;
                }
                continue;
            }
            for &literal in step.literal() {
                if is_line_delimiter(literal) {
                    flush(&mut emit, &run, &mut run_len);
                    emit(b"\r\n");
                    self.col = 0;
                } else if control::is_line_erase(literal) {
                    flush(&mut emit, &run, &mut run_len);
                    if self.col > 0 {
                        emit(&control::ERASE_ECHO);
                        self.col -= 1;
                    }
                } else {
                    if run_len == run.len() {
                        flush(&mut emit, &run, &mut run_len);
                    }
                    run[run_len] = literal;
                    run_len += 1;
                    self.col += 1;
                }
            }
        }
        flush(&mut emit, &run, &mut run_len);
    }
}

/// Emit the accumulated printable run (if any) through `emit` and reset its
/// length. Best-effort, for the echo half.
fn flush<F>(emit: &mut F, run: &[u8], run_len: &mut usize)
where
    F: FnMut(&[u8]),
{
    if *run_len > 0 {
        emit(&run[..*run_len]);
        *run_len = 0;
    }
}

#[cfg(test)]
mod tests;
