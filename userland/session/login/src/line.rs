//! The read line discipline's **buffer** half: assembling one edited input
//! line, a byte at a time, into a caller-provided buffer.
//!
//! A console read returns raw input bytes; turning a stream of keystrokes
//! into a finished line is the read line discipline. The kernel console owns
//! the **echo** half — rendering each character and rubbing one out on a
//! Backspace (`kernel/core::console`, `plans/PI.md` P11). This module is the
//! matching **buffer** half a reader runs: it keeps the line the user is
//! building and applies the same edits to it, so the bytes the reader keeps
//! always match what the screen shows.
//!
//! It is deliberately tiny and seam-free so it is exhaustively testable on the
//! host (the `Run` binary that calls it is a freestanding program built only
//! for the bare-metal targets, `src/run.rs`). It is **allocation-free** — every
//! byte lands in the caller's buffer, because the userland heap is not required
//! to read a keystroke (`plans/SPAWN.md` `SP5b`) — and it
//! recognises the erase control from the one shared `lib/vt` definition, so it can never disagree with the kernel echo about
//! which byte rubs out.

use rustos_vt::control;

/// What feeding one input byte to the read line discipline did.
///
/// The reader loops, calling [`push_line_byte`] for each byte it reads, until
/// it sees a terminal outcome ([`LineFeed::Complete`] or [`LineFeed::TooLong`]).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LineFeed {
    /// The line is still being edited; read another byte.
    Pending,
    /// A line terminator (CR or LF) ended the line. The accumulated length
    /// is the finished line; the terminator itself is not stored.
    Complete,
    /// The byte would have grown the line past the caller's buffer. The read
    /// fails closed rather than truncating a too-long line; the buffer is left holding the bytes accepted so far.
    TooLong,
}

/// Feed one input `byte` into the line being edited in `buf`, with `*len`
/// bytes already accumulated, and report what happened.
///
/// The read line discipline this implements:
///
/// * **CR or LF** ([`control::CR`] / [`control::LF`]) ends the line —
///   [`LineFeed::Complete`]; `*len` is the finished line length and the
///   terminator is not stored (a serial terminal sends CR for Return, a
///   network or local terminal LF, so either terminates).
/// * **Erase** ([`control::is_line_erase`] — Backspace or Delete) rubs out the
///   last accepted byte: `*len` drops by one if the line is non-empty, and the
///   vacated slot is zeroed so a transited credential is not retained. An erase on an empty line is ignored. Either way it is
///   [`LineFeed::Pending`] — an erase never ends the line.
/// * **Any other byte** is appended: it is stored at `buf[*len]` and `*len`
///   grows by one ([`LineFeed::Pending`]) unless the buffer is already full,
///   in which case nothing is stored and the result is [`LineFeed::TooLong`].
///
/// The matching echo (showing the character, or the `BS SP BS` rub-out) is the
/// kernel console's job, so this function performs no I/O; it only edits the
/// buffer. UTF-8 validation of the finished line is the caller's, done once
/// over the whole line.
pub fn push_line_byte(buf: &mut [u8], len: &mut usize, byte: u8) -> LineFeed {
    if byte == control::CR || byte == control::LF {
        return LineFeed::Complete;
    }
    if control::is_line_erase(byte) {
        if *len > 0 {
            *len -= 1;
            buf[*len] = 0;
        }
        return LineFeed::Pending;
    }
    if *len == buf.len() {
        return LineFeed::TooLong;
    }
    buf[*len] = byte;
    *len += 1;
    LineFeed::Pending
}

#[cfg(test)]
mod tests {
    use super::{push_line_byte, LineFeed};

    /// Drive the discipline over a whole byte string, returning the final
    /// outcome and the accumulated line.
    fn feed(bytes: &[u8], cap: usize) -> (LineFeed, alloc::vec::Vec<u8>) {
        let mut buf = alloc::vec![0u8; cap];
        let mut len = 0;
        let mut last = LineFeed::Pending;
        for &b in bytes {
            last = push_line_byte(&mut buf, &mut len, b);
            if matches!(last, LineFeed::Complete | LineFeed::TooLong) {
                break;
            }
        }
        (last, buf[..len].to_vec())
    }

    #[test]
    fn plain_bytes_accumulate_until_a_terminator() {
        let (outcome, line) = feed(b"root\n", 16);
        assert_eq!(outcome, LineFeed::Complete);
        assert_eq!(line, b"root");
    }

    #[test]
    fn cr_terminates_a_line_too() {
        // A serial terminal sends CR for the Return key.
        let (outcome, line) = feed(b"ada\r", 16);
        assert_eq!(outcome, LineFeed::Complete);
        assert_eq!(line, b"ada");
    }

    #[test]
    fn an_empty_line_completes_with_no_bytes() {
        let (outcome, line) = feed(b"\n", 16);
        assert_eq!(outcome, LineFeed::Complete);
        assert!(line.is_empty());
    }

    #[test]
    fn backspace_rubs_out_the_last_byte() {
        // "roox" with the typo deleted, then "t".
        let (outcome, line) = feed(b"roox\x7ft\n", 16);
        assert_eq!(outcome, LineFeed::Complete);
        assert_eq!(line, b"root");
    }

    #[test]
    fn bs_control_also_erases() {
        // A serial terminal's Backspace is BS (`^H`), not DEL.
        let (outcome, line) = feed(b"ab\x08\n", 16);
        assert_eq!(outcome, LineFeed::Complete);
        assert_eq!(line, b"a");
    }

    #[test]
    fn backspace_on_an_empty_line_is_ignored() {
        // Rubbing out nothing leaves an empty line; it must not underflow.
        let (outcome, line) = feed(b"\x7f\x7fok\n", 16);
        assert_eq!(outcome, LineFeed::Complete);
        assert_eq!(line, b"ok");
    }

    #[test]
    fn erasing_then_retyping_to_capacity_is_accepted() {
        // Fill to capacity, rub one out, type one back: still within bounds.
        let (outcome, line) = feed(b"abc\x7fd\n", 3);
        assert_eq!(outcome, LineFeed::Complete);
        assert_eq!(line, b"abd");
    }

    #[test]
    fn an_overlong_line_fails_closed() {
        // A line longer than the buffer is refused, never truncated.
        let (outcome, line) = feed(b"abcd", 3);
        assert_eq!(outcome, LineFeed::TooLong);
        // The bytes accepted before the overflow stay; the caller treats the
        // outcome as a console failure, not a short line.
        assert_eq!(line, b"abc");
    }

    #[test]
    fn the_erased_slot_is_zeroed() {
        // A transited credential byte must not linger in the buffer after an
        // erase.
        let mut buf = [0u8; 8];
        let mut len = 0;
        assert_eq!(push_line_byte(&mut buf, &mut len, b's'), LineFeed::Pending);
        assert_eq!(push_line_byte(&mut buf, &mut len, 0x7f), LineFeed::Pending);
        assert_eq!(len, 0);
        assert_eq!(buf[0], 0);
    }
}
