//! The streaming counting engine: lines, words, characters, bytes, and
//! maximum line display width over an incrementally decoded UTF-8 stream.
//!
//! The rules mirror GNU `wc` in a UTF-8 locale:
//!
//! * **bytes** — the raw stream length.
//! * **lines** — the number of `\n` bytes.
//! * **chars** — decoded characters; a byte that is not part of a valid
//!   UTF-8 sequence is counted as a byte but not as a character.
//! * **words** — maximal runs of non-whitespace characters (Unicode
//!   `White_Space`); an encoding-error byte is non-whitespace.
//! * **max line width** — terminal columns per line: `\t` advances to the
//!   next multiple of 8, `\n`/`\r`/`\x0C` close the line, a space is one
//!   column, other ASCII controls are zero, and every printable character
//!   is measured through the one OS-wide width definition
//!   (`tairix_vt::char_width`).

use tairix_vt::char_width;

/// The five counts of one input.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counts {
    /// Newline count.
    pub lines: u64,
    /// Word count.
    pub words: u64,
    /// Decoded-character count.
    pub chars: u64,
    /// Raw byte count.
    pub bytes: u64,
    /// Maximum display width of a line, in terminal columns.
    pub max_line: u64,
}

impl Counts {
    /// Fold another input's counts into a running total (the `total` row).
    /// Sums saturate; the maximum line width is the maximum of the two.
    pub fn add(&mut self, other: &Counts) {
        self.lines = self.lines.saturating_add(other.lines);
        self.words = self.words.saturating_add(other.words);
        self.chars = self.chars.saturating_add(other.chars);
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.max_line = self.max_line.max(other.max_line);
    }
}

/// The streaming counter: feed it chunks in order, then take the counts.
#[derive(Debug, Default)]
pub struct Counter {
    counts: Counts,
    in_word: bool,
    line_pos: u64,
    /// Bytes of an incomplete UTF-8 sequence carried across chunks.
    pending: [u8; 4],
    pending_len: u8,
}

impl Counter {
    /// A fresh counter for one input.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next chunk of the stream.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.counts.bytes = self.counts.bytes.saturating_add(chunk.len() as u64);
        for &byte in chunk {
            self.feed_byte(byte);
        }
    }

    /// The stream is exhausted: settle the trailing state (an unfinished
    /// UTF-8 sequence is error bytes; the last line's width still counts)
    /// and return the totals.
    #[must_use]
    pub fn finish(mut self) -> Counts {
        let trailing = self.pending_len;
        self.pending_len = 0;
        for _ in 0..trailing {
            self.error_byte();
        }
        if self.line_pos > self.counts.max_line {
            self.counts.max_line = self.line_pos;
        }
        self.counts
    }

    fn feed_byte(&mut self, byte: u8) {
        if self.pending_len == 0 {
            if byte < 0x80 {
                self.accept(char::from(byte));
                return;
            }
            if utf8_len(byte) == 0 {
                // A stray continuation or invalid lead byte.
                self.error_byte();
                return;
            }
            self.pending[0] = byte;
            self.pending_len = 1;
            return;
        }

        // Inside a multi-byte sequence: only a continuation byte extends it.
        if byte & 0xC0 != 0x80 {
            self.flush_pending_as_errors();
            self.feed_byte(byte);
            return;
        }
        let len = usize::from(self.pending_len);
        self.pending[len] = byte;
        self.pending_len += 1;
        let expected = utf8_len(self.pending[0]);
        if self.pending_len == expected {
            let seq = &self.pending[..usize::from(expected)];
            match core::str::from_utf8(seq)
                .ok()
                .and_then(|s| s.chars().next())
            {
                Some(ch) => {
                    self.pending_len = 0;
                    self.accept(ch);
                }
                // Well-formed length but an invalid scalar (surrogate or
                // overlong encoding): every byte is an error byte.
                None => self.flush_pending_as_errors(),
            }
        }
    }

    /// Classify one decoded character.
    fn accept(&mut self, ch: char) {
        self.counts.chars = self.counts.chars.saturating_add(1);
        match ch {
            '\n' => {
                self.counts.lines = self.counts.lines.saturating_add(1);
                self.close_line();
            }
            '\r' | '\x0C' => self.close_line(),
            '\t' => {
                self.line_pos = self.line_pos.saturating_add(8 - (self.line_pos % 8));
                self.in_word = false;
            }
            ' ' => {
                self.line_pos = self.line_pos.saturating_add(1);
                self.in_word = false;
            }
            '\x0B' => self.in_word = false,
            _ => {
                if ch.is_ascii() {
                    // Remaining ASCII controls (and DEL) occupy no columns.
                    if ch.is_ascii_graphic() {
                        self.line_pos = self.line_pos.saturating_add(1);
                    }
                } else {
                    self.line_pos = self.line_pos.saturating_add(u64::from(char_width(ch)));
                }
                let word = !ch.is_whitespace();
                if word && !self.in_word {
                    self.counts.words = self.counts.words.saturating_add(1);
                }
                self.in_word = word;
            }
        }
    }

    /// A byte that is not part of any character: counted as a byte only,
    /// treated as non-whitespace for word boundaries, zero columns wide.
    fn error_byte(&mut self) {
        if !self.in_word {
            self.counts.words = self.counts.words.saturating_add(1);
        }
        self.in_word = true;
    }

    fn flush_pending_as_errors(&mut self) {
        let pending = self.pending_len;
        self.pending_len = 0;
        for _ in 0..pending {
            self.error_byte();
        }
    }

    fn close_line(&mut self) {
        if self.line_pos > self.counts.max_line {
            self.counts.max_line = self.line_pos;
        }
        self.line_pos = 0;
        self.in_word = false;
    }
}

/// The byte length a UTF-8 lead byte announces, or `0` for a byte that
/// cannot start a sequence.
fn utf8_len(lead: u8) -> u8 {
    match lead {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{Counter, Counts};

    /// Count `data` fed in `chunk`-sized pieces.
    fn count_chunked(data: &[u8], chunk: usize) -> Counts {
        let mut counter = Counter::new();
        for piece in data.chunks(chunk.max(1)) {
            counter.feed(piece);
        }
        counter.finish()
    }

    fn count(data: &[u8]) -> Counts {
        count_chunked(data, data.len().max(1))
    }

    #[test]
    fn the_classic_ascii_counts() {
        let counts = count(b"one two three\nfour five\n");
        assert_eq!(counts.lines, 2);
        assert_eq!(counts.words, 5);
        assert_eq!(counts.chars, 24);
        assert_eq!(counts.bytes, 24);
        assert_eq!(counts.max_line, 13);
    }

    #[test]
    fn empty_input_counts_nothing() {
        assert_eq!(count(b""), Counts::default());
    }

    #[test]
    fn an_unterminated_final_line_adds_no_newline_but_counts_width() {
        let counts = count(b"ab\ncdef");
        assert_eq!(counts.lines, 1);
        assert_eq!(counts.words, 2);
        assert_eq!(counts.max_line, 4);
    }

    #[test]
    fn tabs_advance_to_eight_column_stops() {
        // "a\tb": column 1, tab to 8, then one more = 9.
        assert_eq!(count(b"a\tb\n").max_line, 9);
        // A tab at a stop advances a full stop.
        assert_eq!(count(b"12345678\tx\n").max_line, 17);
    }

    #[test]
    fn carriage_return_and_form_feed_close_the_line() {
        assert_eq!(count(b"abcdef\rxy\n").max_line, 6);
        assert_eq!(count(b"ab\x0Cxyzw\n").max_line, 4);
    }

    #[test]
    fn ascii_controls_are_wordy_but_zero_width() {
        // A BEL between letters neither breaks the word nor adds width.
        let counts = count(b"a\x07b\n");
        assert_eq!(counts.words, 1);
        assert_eq!(counts.max_line, 2);
        // A vertical tab breaks the word without adding width.
        let counts = count(b"a\x0Bb\n");
        assert_eq!(counts.words, 2);
        assert_eq!(counts.max_line, 2);
    }

    #[test]
    fn multibyte_characters_count_once_each() {
        let text = "héllo wörld\n".as_bytes();
        let counts = count(text);
        assert_eq!(counts.chars, 12);
        assert_eq!(counts.bytes, 14);
        assert_eq!(counts.words, 2);
        assert_eq!(counts.max_line, 11);
    }

    #[test]
    fn wide_characters_occupy_two_columns() {
        let counts = count("日本\n".as_bytes());
        assert_eq!(counts.chars, 3);
        assert_eq!(counts.words, 1);
        assert_eq!(counts.max_line, 4);
    }

    #[test]
    fn unicode_whitespace_separates_words() {
        // A no-break space is whitespace here, as in GNU wc.
        let counts = count("a\u{00A0}b\n".as_bytes());
        assert_eq!(counts.words, 2);
    }

    #[test]
    fn encoding_errors_are_bytes_but_not_chars() {
        // A stray continuation byte and an overlong lead.
        let counts = count(b"a\x80b\n");
        assert_eq!(counts.bytes, 4);
        assert_eq!(counts.chars, 3);
        // The error byte glues the word together (non-whitespace).
        assert_eq!(counts.words, 1);
        // An isolated error byte between spaces is its own word.
        let counts = count(b" \xC0 \n");
        assert_eq!(counts.words, 1);
        assert_eq!(counts.chars, 3);
    }

    #[test]
    fn a_truncated_sequence_at_eof_is_error_bytes() {
        // The first two bytes of a three-byte sequence, then EOF.
        let counts = count(b"a\xE2\x82");
        assert_eq!(counts.bytes, 3);
        assert_eq!(counts.chars, 1);
        assert_eq!(counts.words, 1);
    }

    #[test]
    fn chunk_boundaries_never_change_the_counts() {
        let data = "日本 héllo\tworld\r\nsecond line\nno-newline tail é".as_bytes();
        let reference = count(data);
        for chunk in 1..=data.len() {
            assert_eq!(
                count_chunked(data, chunk),
                reference,
                "chunk size {chunk} diverged"
            );
        }
    }

    #[test]
    fn totals_fold_with_saturation_and_max_width() {
        let mut total = Counts::default();
        total.add(&Counts {
            lines: 1,
            words: 2,
            chars: 3,
            bytes: 4,
            max_line: 9,
        });
        total.add(&Counts {
            lines: u64::MAX,
            words: 1,
            chars: 1,
            bytes: 1,
            max_line: 5,
        });
        assert_eq!(total.lines, u64::MAX);
        assert_eq!(total.words, 3);
        assert_eq!(total.max_line, 9);
    }
}
