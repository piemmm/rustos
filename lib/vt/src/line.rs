//! The read line discipline's **buffer** half: assembling one edited input
//! line, a byte at a time, into a caller-provided buffer.
//!
//! A console read returns raw input bytes; turning a stream of keystrokes
//! into a finished line is the read line discipline. The kernel console owns
//! the **echo** half — rendering each character and rubbing one out on an
//! erase (`kernel/core::console`, `plans/PI.md` P11). This module is the
//! matching **buffer** half every console reader runs (login's prompt reads,
//! the shell REPL): it keeps the line the user is building and applies the
//! same edits to it, so the bytes the reader keeps always match what the
//! screen shows. It lives here, beside the [`control`] vocabulary both halves
//! key off, so there is exactly one definition of which byte terminates a
//! line and which input rubs one out — a reader with a private copy could
//! silently disagree with the kernel echo.
//!
//! An erase is not always a single byte: the Delete key arrives as the
//! multi-byte `CSI 3 ~` sequence ([`crate::Key::Delete`]), which may be split across
//! reads. [`EraseSeq`] is the incremental recogniser for it, and
//! [`LineEditor`] carries that state across bytes — one editor per line being
//! read. Both halves of the discipline run the same recogniser, so screen and
//! buffer can never disagree about what the Delete key did.
//!
//! It is deliberately tiny and seam-free so it is exhaustively testable on
//! the host. It is **allocation-free** — every byte lands in the caller's
//! buffer, because the userland heap is not required to read a keystroke
//! (`plans/SPAWN.md` `SP5b`) — and it recognises the erase controls from the
//! one shared [`control`] / [`crate::Key`] definition.

use crate::control;

/// What feeding one input byte to the read line discipline did.
///
/// The reader loops, calling [`LineEditor::push`] for each byte it reads,
/// until it sees a terminal outcome ([`LineFeed::Complete`] or
/// [`LineFeed::TooLong`]).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LineFeed {
    /// The line is still being edited; read another byte.
    Pending,
    /// A line terminator (CR or LF) ended the line. The accumulated length
    /// is the finished line; the terminator itself is not stored.
    Complete,
    /// The byte would have grown the line past the caller's buffer. The read
    /// fails closed rather than truncating a too-long line; the buffer is
    /// left holding the bytes accepted so far.
    TooLong,
    /// A lone `ESC` (not the start of a CSI sequence) opened the line.
    ///
    /// `ESC` also introduces the editing CSI sequences (the Delete key's
    /// `CSI 3 ~`, the arrow keys), so a bare `ESC` can only be told apart
    /// from `ESC [ …` once the reader has looked for a follow-on `[` — a
    /// bounded, timed re-poll it drives, since the buffer half moves no
    /// bytes of its own. When that re-poll resolves the held `ESC` as lone
    /// (no follow-on `[` arrives), the reader calls
    /// [`LineEditor::resolve_escape`], which reports it here.
    ///
    /// A reader that has no meaning for a bare `ESC` never calls
    /// [`resolve_escape`](LineEditor::resolve_escape), so it never sees this
    /// outcome; the pre-boot unlock reader uses it to drop into the
    /// Supervisor console when `ESC` opens the line.
    Escape,
}

/// The escape sequence the Delete key sends (`CSI 3 ~`), spelled from the
/// shared [`control`] introducers — the one encoding `lib/vt`'s emitter,
/// parser ([`crate::Key::Delete`]), and `lib/keymap` all share. The parser
/// round-trip test pins this spelling to the key vocabulary, so the
/// recogniser can never silently drift from it.
const DELETE_SEQ: [u8; 4] = [control::ESC, control::CSI, b'3', control::TILDE];

/// What feeding one byte to the [`EraseSeq`] recogniser produced.
///
/// `literal` holds the bytes the caller must now treat as ordinary input: a
/// previously-held sequence prefix that failed to complete, and (unless the
/// byte re-opened a match) the byte just fed. `erase` reports that the full
/// Delete sequence completed — exactly one rub-out, with no literal bytes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SeqFeed {
    literal: [u8; DELETE_SEQ.len()],
    literal_len: u8,
    erase: bool,
}

impl SeqFeed {
    /// The bytes to treat as ordinary (non-erase) input, in arrival order.
    #[must_use]
    pub fn literal(&self) -> &[u8] {
        &self.literal[..usize::from(self.literal_len)]
    }

    /// Whether the full Delete sequence completed: rub out one character.
    #[must_use]
    pub const fn erase(&self) -> bool {
        self.erase
    }
}

/// Incremental recogniser for the Delete key's `CSI 3 ~` escape sequence,
/// byte at a time.
///
/// The sequence may be split across console reads, so the recogniser holds
/// the matched prefix between calls. A byte that breaks the match releases
/// the held prefix as literal input (never silently dropped) and — when the
/// breaking byte is itself `ESC` — immediately re-opens a fresh match, so
/// `ESC ESC [ 3 ~` still erases once and passes one literal `ESC` through.
///
/// Both halves of the read line discipline run one of these (the kernel echo
/// and every [`LineEditor`]), fed the same bytes in the same order, so they
/// agree by construction about which bytes were an erase.
#[derive(Debug, Default)]
pub struct EraseSeq {
    /// How many leading bytes of [`DELETE_SEQ`] are currently held.
    matched: u8,
}

impl EraseSeq {
    /// A fresh recogniser holding no prefix.
    #[must_use]
    pub const fn new() -> Self {
        Self { matched: 0 }
    }

    /// Whether the recogniser is currently holding exactly a lone `ESC`
    /// prefix — the one byte both the Delete/arrow CSI sequences and a bare
    /// `ESC` keystroke begin with, not yet disambiguated by a follow-on byte.
    #[must_use]
    pub const fn holding_escape(&self) -> bool {
        self.matched == 1
    }

    /// Drop any held prefix, returning to the fresh state. Used when a
    /// reader has resolved a held lone `ESC` itself.
    pub fn reset(&mut self) {
        self.matched = 0;
    }

    /// Feed one input byte; see [`SeqFeed`] for what the caller must do.
    pub fn feed(&mut self, byte: u8) -> SeqFeed {
        let mut out = SeqFeed {
            literal: [0; DELETE_SEQ.len()],
            literal_len: 0,
            erase: false,
        };
        let matched = usize::from(self.matched);
        if byte == DELETE_SEQ[matched] {
            if matched + 1 == DELETE_SEQ.len() {
                self.matched = 0;
                out.erase = true;
            } else {
                self.matched = self.matched.wrapping_add(1);
            }
            return out;
        }
        // The match broke: release the held prefix as literal input, then
        // either re-open on a fresh `ESC` or pass the byte through too.
        out.literal[..matched].copy_from_slice(&DELETE_SEQ[..matched]);
        out.literal_len = self.matched;
        if byte == DELETE_SEQ[0] {
            self.matched = 1;
        } else {
            out.literal[matched] = byte;
            out.literal_len = out.literal_len.wrapping_add(1);
            self.matched = 0;
        }
        out
    }
}

/// The read line discipline's buffer half, with the [`EraseSeq`] state one
/// edited line needs. Create one per line read; drop it when the line
/// completes.
#[derive(Debug, Default)]
pub struct LineEditor {
    seq: EraseSeq,
}

impl LineEditor {
    /// A fresh editor for one line.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            seq: EraseSeq::new(),
        }
    }

    /// Feed one input `byte` into the line being edited in `buf`, with
    /// `*len` bytes already accumulated, and report what happened.
    ///
    /// The read line discipline this implements:
    ///
    /// * **CR or LF** ([`control::CR`] / [`control::LF`]) ends the line —
    ///   [`LineFeed::Complete`]; `*len` is the finished line length and the
    ///   terminator is not stored (a serial terminal sends CR for Return, a
    ///   network or local terminal LF, so either terminates).
    /// * **Erase** — Backspace or Delete as a single byte
    ///   ([`control::is_line_erase`]), or the Delete key's `CSI 3 ~`
    ///   sequence ([`EraseSeq`]) — rubs out the last accepted byte: `*len`
    ///   drops by one if the line is non-empty, and the vacated slot is
    ///   zeroed so a transited credential is not retained. An erase on an
    ///   empty line is ignored. Either way it is [`LineFeed::Pending`] — an
    ///   erase never ends the line.
    /// * **Any other byte** is appended: it is stored at `buf[*len]` and
    ///   `*len` grows by one ([`LineFeed::Pending`]) unless the buffer is
    ///   already full, in which case nothing is stored and the result is
    ///   [`LineFeed::TooLong`].
    ///
    /// The matching echo (showing the character, or the `BS SP BS` rub-out)
    /// is the kernel console's job, so this function performs no I/O; it
    /// only edits the buffer. UTF-8 validation of the finished line is the
    /// caller's, done once over the whole line.
    pub fn push(&mut self, buf: &mut [u8], len: &mut usize, byte: u8) -> LineFeed {
        let step = self.seq.feed(byte);
        if step.erase() {
            erase_last(buf, len);
            return LineFeed::Pending;
        }
        for &literal in step.literal() {
            match push_literal(buf, len, literal) {
                LineFeed::Pending => {}
                terminal => return terminal,
            }
        }
        LineFeed::Pending
    }

    /// Resolve a currently-held lone `ESC` when the reader's bounded
    /// re-poll for a follow-on byte timed out.
    ///
    /// After feeding `ESC` the discipline holds it, since it may still open
    /// an editing CSI sequence (`ESC [ …`). A reader that wants to act on a
    /// bare `ESC` does one short, bounded, timed re-poll for the next byte;
    /// if none arrives it calls this. When a lone `ESC` is held on an
    /// otherwise-empty line this clears the held state and reports
    /// [`LineFeed::Escape`]; otherwise (no `ESC` held, or the line already
    /// has content) it reports [`LineFeed::Pending`] and changes nothing.
    pub fn resolve_escape(&mut self, len: usize) -> LineFeed {
        if len == 0 && self.seq.holding_escape() {
            self.seq.reset();
            return LineFeed::Escape;
        }
        LineFeed::Pending
    }
}

/// Rub out the last accepted byte of the line, zeroing the vacated slot so a
/// transited credential is not retained. A no-op on an empty line.
fn erase_last(buf: &mut [u8], len: &mut usize) {
    if *len > 0 {
        *len -= 1;
        buf[*len] = 0;
    }
}

/// Apply one already-disambiguated byte to the line (the per-byte core the
/// sequence layer feeds).
fn push_literal(buf: &mut [u8], len: &mut usize, byte: u8) -> LineFeed {
    if byte == control::CR || byte == control::LF {
        return LineFeed::Complete;
    }
    if control::is_line_erase(byte) {
        erase_last(buf, len);
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
    use super::{EraseSeq, LineEditor, LineFeed, DELETE_SEQ};

    /// Drive the discipline over a whole byte string, returning the final
    /// outcome and the accumulated line.
    fn feed(bytes: &[u8], cap: usize) -> (LineFeed, alloc::vec::Vec<u8>) {
        let mut editor = LineEditor::new();
        let mut buf = alloc::vec![0u8; cap];
        let mut len = 0;
        let mut last = LineFeed::Pending;
        for &b in bytes {
            last = editor.push(&mut buf, &mut len, b);
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
    fn the_delete_key_sequence_erases_like_backspace() {
        // The Delete key arrives as `CSI 3 ~`, not a single byte; it must
        // rub out exactly one character, never land in the line as raw
        // escape bytes.
        let (outcome, line) = feed(b"roox\x1b[3~t\n", 16);
        assert_eq!(outcome, LineFeed::Complete);
        assert_eq!(line, b"root");
    }

    #[test]
    fn the_delete_key_sequence_erases_across_split_reads() {
        // A console reader drains a byte at a time, so the sequence is
        // always split; the editor's held state must survive the splits.
        let mut editor = LineEditor::new();
        let mut buf = [0u8; 8];
        let mut len = 0;
        for &b in b"ab" {
            assert_eq!(editor.push(&mut buf, &mut len, b), LineFeed::Pending);
        }
        for &b in &DELETE_SEQ {
            assert_eq!(editor.push(&mut buf, &mut len, b), LineFeed::Pending);
        }
        assert_eq!(len, 1);
        assert_eq!(&buf[..len], b"a");
        // The vacated slot is zeroed.
        assert_eq!(buf[1], 0);
    }

    #[test]
    fn delete_on_an_empty_line_is_ignored() {
        let (outcome, line) = feed(b"\x1b[3~ok\n", 16);
        assert_eq!(outcome, LineFeed::Complete);
        assert_eq!(line, b"ok");
    }

    #[test]
    fn a_broken_delete_prefix_lands_as_literal_bytes() {
        // `ESC [ 4 ~` is the End key, not Delete: the held prefix and the
        // breaking bytes pass through as ordinary input (the discipline
        // does not interpret other sequences), never a silent drop.
        let (outcome, line) = feed(b"\x1b[4~\n", 16);
        assert_eq!(outcome, LineFeed::Complete);
        assert_eq!(line, b"\x1b[4~");
    }

    #[test]
    fn an_esc_that_breaks_a_prefix_reopens_the_match() {
        // `ESC ESC [ 3 ~`: the first ESC is released as a literal into the
        // line, and the second still completes a Delete — which then rubs
        // that literal ESC back out, exactly as Delete erases any last
        // character.
        let (outcome, line) = feed(b"ab\x1b\x1b[3~\n", 16);
        assert_eq!(outcome, LineFeed::Complete);
        assert_eq!(line, b"ab");
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
        let mut editor = LineEditor::new();
        let mut buf = [0u8; 8];
        let mut len = 0;
        assert_eq!(editor.push(&mut buf, &mut len, b's'), LineFeed::Pending);
        assert_eq!(editor.push(&mut buf, &mut len, 0x7f), LineFeed::Pending);
        assert_eq!(len, 0);
        assert_eq!(buf[0], 0);
    }

    #[test]
    fn the_delete_sequence_matches_the_shared_key_vocabulary() {
        // The recogniser's spelling and the emitter/parser vocabulary must
        // be one definition: the sequence parses back to `Key::Delete`.
        let mut parser = crate::Parser::new();
        let mut seen = alloc::vec::Vec::new();
        parser.feed(&DELETE_SEQ, |op| seen.push(op));
        assert_eq!(seen, alloc::vec![crate::Op::Key(crate::Key::Delete)]);
    }

    #[test]
    fn a_lone_esc_is_held_then_resolves_to_escape_on_timeout() {
        // Feeding `ESC` alone holds it (it may still open `ESC [ …`); the
        // reader's bounded re-poll then times out and resolves it as a lone
        // ESC. No byte lands in the line.
        let mut editor = LineEditor::new();
        let mut buf = [0u8; 8];
        let mut len = 0;
        assert_eq!(
            editor.push(&mut buf, &mut len, super::control::ESC),
            LineFeed::Pending
        );
        assert!(editor.seq.holding_escape());
        assert_eq!(editor.resolve_escape(len), LineFeed::Escape);
        assert_eq!(len, 0);
        // The held state is cleared, so a stray follow-up resolve is a no-op.
        assert_eq!(editor.resolve_escape(len), LineFeed::Pending);
    }

    #[test]
    fn esc_then_bracket_still_edits_and_never_resolves_to_escape() {
        // `ESC [ 3 ~` (Delete) must keep erasing; the follow-on `[` means the
        // held ESC was a CSI introducer, never a lone ESC.
        let (outcome, line) = feed(b"roox\x1b[3~t\n", 16);
        assert_eq!(outcome, LineFeed::Complete);
        assert_eq!(line, b"root");
    }

    #[test]
    fn resolve_escape_on_a_non_empty_line_changes_nothing() {
        // A held ESC only means "lone ESC" when the line is empty; with
        // content already typed, resolving is a no-op (the ESC stays held for
        // the reader's next byte, exactly as before).
        let mut editor = LineEditor::new();
        let mut buf = [0u8; 8];
        let mut len = 0;
        assert_eq!(editor.push(&mut buf, &mut len, b'a'), LineFeed::Pending);
        assert_eq!(
            editor.push(&mut buf, &mut len, super::control::ESC),
            LineFeed::Pending
        );
        assert_eq!(editor.resolve_escape(len), LineFeed::Pending);
        assert_eq!(len, 1);
    }

    #[test]
    fn resolve_escape_without_a_held_esc_is_a_no_op() {
        let mut editor = LineEditor::new();
        assert_eq!(editor.resolve_escape(0), LineFeed::Pending);
    }

    #[test]
    fn a_bare_erase_seq_reports_erase_with_no_literals() {
        let mut seq = EraseSeq::new();
        let mut outcomes = alloc::vec::Vec::new();
        for &b in &DELETE_SEQ {
            outcomes.push(seq.feed(b));
        }
        for step in &outcomes[..DELETE_SEQ.len() - 1] {
            assert!(!step.erase());
            assert!(step.literal().is_empty());
        }
        let last = &outcomes[DELETE_SEQ.len() - 1];
        assert!(last.erase());
        assert!(last.literal().is_empty());
    }
}
