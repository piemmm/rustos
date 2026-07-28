//! Rich-screen presentation for the Supervisor: colour, cursor positioning,
//! and the alternate screen, built entirely on the shared `lib/vt`
//! vocabulary.
//!
//! At the bootstrap floor a rich screen costs nothing new: the console is
//! already a byte stream that consumes escape sequences (the boot-screen
//! state machine already emits `\r` and `\x1b[K`), and `lib/vt` already has a
//! complete, arch-neutral, allocation-free VT emitter ([`tairix_vt::emit`])
//! whose output round-trips through its parser. This module is a thin
//! [`Op`]-building layer over that emitter — it never
//! hand-rolls a second copy of the CSI/SGR encoding (the charter forbids the
//! duplication) and it names no board, MMIO, or architecture.
//!
//! # Degrade gracefully
//!
//! Colour and positioning are a nicety, never a correctness dependency. A
//! [`Screen`] built with `plain` set emits **no** escape bytes at all — only
//! the text — so a genuinely dumb serial line still shows usable output. The
//! choice is a single injected flag; there is no probe (the write seam is
//! one-way and the `TERM`/`lib/termcap` database lives on the not-yet-mounted
//! `/System`).
//!
//! # Bounded, fail-closed geometry
//!
//! With no way to query the console's size, a full-screen layout assumes a
//! conservative [`Geometry`] (80×24 by default) threaded in as data, never a
//! per-board constant. Every position is clamped into that geometry, so a
//! malformed or oversized coordinate is pinned to the edge rather than
//! positioning off-screen — and nothing here panics on any input.

use tairix_vt::attr::Sgr;
use tairix_vt::color::Color;
use tairix_vt::emit::encode_into;
use tairix_vt::op::{EraseMode, Op};

use crate::Report;

/// The console geometry a full-screen layout assumes.
///
/// The bootstrap-floor console cannot be queried for its size, so a layout
/// assumes a conservative default ([`Geometry::DEFAULT`], 80×24) unless a real
/// value is threaded in from discovery. [`Screen`] clamps every position into
/// this geometry, so a layout never addresses a cell outside it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Geometry {
    /// Number of columns (at least 1).
    pub cols: u16,
    /// Number of rows (at least 1).
    pub rows: u16,
}

impl Geometry {
    /// The conservative default assumed when the real console size is unknown.
    pub const DEFAULT: Geometry = Geometry { cols: 80, rows: 24 };

    /// A geometry of `cols` × `rows`, each clamped up to at least 1 so a
    /// degenerate size never yields a zero-dimension (fail-closed).
    #[must_use]
    pub const fn new(cols: u16, rows: u16) -> Geometry {
        Geometry {
            cols: if cols == 0 { 1 } else { cols },
            rows: if rows == 0 { 1 } else { rows },
        }
    }
}

impl Default for Geometry {
    fn default() -> Self {
        Geometry::DEFAULT
    }
}

/// A text rendition: foreground/background colour plus the common attributes.
///
/// A [`Style`] maps to a deterministic sequence of [`Sgr`] operations that
/// [`Screen::set_style`] emits through the shared encoder. It always begins
/// with [`Sgr::Reset`] so applying a style fully
/// replaces the previous rendition rather than layering onto it, and both
/// colours are always emitted ([`Color::Default`] renders as the terminal's
/// default, SGR `39`/`49`), so the on-wire result is independent of whatever
/// came before.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Style {
    /// Foreground colour.
    pub fg: Color,
    /// Background colour.
    pub bg: Color,
    /// Bold / increased intensity.
    pub bold: bool,
    /// Underline.
    pub underline: bool,
    /// Reverse video (swap foreground and background).
    pub reverse: bool,
}

/// The most [`Sgr`] operations a single [`Style`] expands to: reset, three
/// attribute flags, and the two colours.
const STYLE_MAX_SGR: usize = 6;

impl Style {
    /// The plain default rendition: default colours, no attributes.
    pub const DEFAULT: Style = Style {
        fg: Color::Default,
        bg: Color::Default,
        bold: false,
        underline: false,
        reverse: false,
    };

    /// A style with foreground colour `fg` on the default background.
    #[must_use]
    pub const fn fg(fg: Color) -> Style {
        Style {
            fg,
            ..Style::DEFAULT
        }
    }

    /// This style with the background set to `bg`.
    #[must_use]
    pub const fn on(mut self, bg: Color) -> Style {
        self.bg = bg;
        self
    }

    /// This style made bold.
    #[must_use]
    pub const fn bold(mut self) -> Style {
        self.bold = true;
        self
    }

    /// This style underlined.
    #[must_use]
    pub const fn underline(mut self) -> Style {
        self.underline = true;
        self
    }

    /// This style in reverse video.
    #[must_use]
    pub const fn reverse(mut self) -> Style {
        self.reverse = true;
        self
    }

    /// Write this style's canonical [`Sgr`] operations into `buf`, returning
    /// how many were produced.
    ///
    /// The order is fixed — reset, bold, underline, reverse, foreground,
    /// background — so the emitted bytes are deterministic and match the
    /// `lib/vt` encoding of the same operations.
    fn sgr_ops(&self, buf: &mut [Sgr; STYLE_MAX_SGR]) -> usize {
        let mut n = 0;
        buf[n] = Sgr::Reset;
        n += 1;
        if self.bold {
            buf[n] = Sgr::Bold;
            n += 1;
        }
        if self.underline {
            buf[n] = Sgr::Underline;
            n += 1;
        }
        if self.reverse {
            buf[n] = Sgr::Reverse;
            n += 1;
        }
        buf[n] = Sgr::Foreground(self.fg);
        n += 1;
        buf[n] = Sgr::Background(self.bg);
        n += 1;
        n
    }
}

impl Default for Style {
    fn default() -> Self {
        Style::DEFAULT
    }
}

/// The bytes of one escape sequence buffered on the stack before a single
/// [`Report::write_bytes`] call.
///
/// A fixed chunk keeps the presenter allocation-free and off the per-byte
/// write path (the sink flushes when the chunk fills, so no sequence ever
/// overflows it); the value comfortably holds the longest operation this
/// module emits (a truecolour SGR is under 20 bytes).
const SINK_CHUNK: usize = 64;

/// An `Extend<u8>` adapter that funnels the encoder's bytes into a
/// [`Report`], buffering a chunk at a time.
///
/// [`encode_into`] writes into any
/// `Extend<u8>`; this bridges that to the object-safe [`Report`] seam without
/// a per-byte call and without ever overflowing (a full buffer is flushed
/// before the next byte is stored).
struct ReportSink<'a> {
    out: &'a mut dyn Report,
    buf: [u8; SINK_CHUNK],
    len: usize,
}

impl<'a> ReportSink<'a> {
    fn new(out: &'a mut dyn Report) -> Self {
        Self {
            out,
            buf: [0; SINK_CHUNK],
            len: 0,
        }
    }

    fn flush(&mut self) {
        if self.len > 0 {
            self.out.write_bytes(&self.buf[..self.len]);
            self.len = 0;
        }
    }
}

impl Extend<u8> for ReportSink<'_> {
    fn extend<T: IntoIterator<Item = u8>>(&mut self, iter: T) {
        for byte in iter {
            if self.len == self.buf.len() {
                self.flush();
            }
            self.buf[self.len] = byte;
            self.len += 1;
        }
    }
}

/// A colour/positioning presenter over a [`Report`], built on `lib/vt`.
///
/// Every escape sequence is produced by constructing an
/// [`Op`] and encoding it through
/// [`encode_into`], so this crate carries no
/// second copy of the terminal encoding. When `plain` is set the control
/// helpers emit nothing (only the text helpers write), so a dumb serial line
/// still shows usable output. Positions are clamped into the presenter's
/// [`Geometry`]; no method panics on any input.
pub struct Screen<'a> {
    out: &'a mut dyn Report,
    geometry: Geometry,
    plain: bool,
}

impl<'a> Screen<'a> {
    /// A presenter writing to `out` with the given `geometry`. When `plain`
    /// is `true` the control helpers emit no escape bytes.
    pub fn new(out: &'a mut dyn Report, geometry: Geometry, plain: bool) -> Self {
        Self {
            out,
            geometry,
            plain,
        }
    }

    /// Whether this presenter is in plain (escape-free) mode.
    #[must_use]
    pub fn is_plain(&self) -> bool {
        self.plain
    }

    /// The geometry positions are clamped into.
    #[must_use]
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// Write raw text (both modes).
    pub fn write_str(&mut self, text: &str) {
        self.out.write_bytes(text.as_bytes());
    }

    /// Write raw bytes (both modes).
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.out.write_bytes(bytes);
    }

    /// End the current line with CR-LF (both modes).
    pub fn newline(&mut self) {
        self.out.write_bytes(b"\r\n");
    }

    /// Emit `op` through the shared encoder, unless in plain mode.
    fn emit(&mut self, op: &Op) {
        if self.plain {
            return;
        }
        let mut sink = ReportSink::new(self.out);
        encode_into(op, &mut sink);
        sink.flush();
    }

    /// Emit each `op` in order through the shared encoder, unless in plain
    /// mode. One buffered flush covers the whole run.
    fn emit_all(&mut self, ops: &[Op]) {
        if self.plain {
            return;
        }
        let mut sink = ReportSink::new(self.out);
        for op in ops {
            encode_into(op, &mut sink);
        }
        sink.flush();
    }

    /// Move the cursor to 1-based `row`, `col`, clamped into the geometry so
    /// the position is never off-screen (a `0` clamps up to `1`; an
    /// over-large value clamps to the last row/column).
    pub fn move_to(&mut self, row: u16, col: u16) {
        let row = row.clamp(1, self.geometry.rows);
        let col = col.clamp(1, self.geometry.cols);
        self.emit(&Op::CursorPosition { row, col });
    }

    /// Clear the whole display and home the cursor.
    pub fn clear(&mut self) {
        self.emit_all(&[
            Op::CursorPosition { row: 1, col: 1 },
            Op::EraseInDisplay(EraseMode::All),
        ]);
    }

    /// Apply `style` as the current rendition (reset then set).
    pub fn set_style(&mut self, style: &Style) {
        let mut buf = [Sgr::Reset; STYLE_MAX_SGR];
        let count = style.sgr_ops(&mut buf);
        if self.plain {
            return;
        }
        let mut sink = ReportSink::new(self.out);
        for sgr in &buf[..count] {
            encode_into(&Op::Sgr(*sgr), &mut sink);
        }
        sink.flush();
    }

    /// Reset every rendition attribute to the terminal default.
    pub fn reset_style(&mut self) {
        self.emit(&Op::Sgr(Sgr::Reset));
    }

    /// Enter the alternate screen and hide the cursor, then clear — the
    /// full-screen takeover a memtest-style UI wants. In plain mode this is a
    /// no-op.
    pub fn enter_fullscreen(&mut self) {
        self.emit_all(&[
            Op::EnterAltScreen,
            Op::HideCursor,
            Op::CursorPosition { row: 1, col: 1 },
            Op::EraseInDisplay(EraseMode::All),
        ]);
    }

    /// Show the cursor and leave the alternate screen, restoring the previous
    /// display. In plain mode this is a no-op.
    pub fn leave_fullscreen(&mut self) {
        self.emit_all(&[Op::ShowCursor, Op::LeaveAltScreen]);
    }
}

#[cfg(test)]
mod tests {
    use super::{Geometry, ReportSink, Screen, Style};
    use crate::commands::test_support::VecReport;
    use tairix_vt::attr::Sgr;
    use tairix_vt::color::{BasicColor, Color};
    use tairix_vt::emit::encode_into;
    use tairix_vt::op::{EraseMode, Op};

    /// Encode a run of `Op`s to bytes the way `lib/vt` does, for the
    /// "never a second copy of the encoding" assertions.
    fn encode(ops: &[Op]) -> alloc::vec::Vec<u8> {
        let mut bytes = alloc::vec::Vec::new();
        for op in ops {
            encode_into(op, &mut bytes);
        }
        bytes
    }

    #[test]
    fn geometry_new_clamps_zero_dimensions_up_to_one() {
        let g = Geometry::new(0, 0);
        assert_eq!(g, Geometry { cols: 1, rows: 1 });
    }

    #[test]
    fn move_to_matches_the_vt_encoding() {
        let mut out = VecReport::default();
        let mut screen = Screen::new(&mut out, Geometry::DEFAULT, false);
        screen.move_to(10, 20);
        assert_eq!(
            out.bytes(),
            encode(&[Op::CursorPosition { row: 10, col: 20 }])
        );
    }

    #[test]
    fn move_to_clamps_into_the_geometry() {
        let mut out = VecReport::default();
        let mut screen = Screen::new(&mut out, Geometry::new(80, 24), false);
        // Row 0 clamps up to 1; over-large row/col clamp to the last cell.
        screen.move_to(0, 0);
        screen.move_to(999, 999);
        let expected = encode(&[
            Op::CursorPosition { row: 1, col: 1 },
            Op::CursorPosition { row: 24, col: 80 },
        ]);
        assert_eq!(out.bytes(), expected);
    }

    #[test]
    fn clear_matches_the_vt_encoding() {
        let mut out = VecReport::default();
        let mut screen = Screen::new(&mut out, Geometry::DEFAULT, false);
        screen.clear();
        let expected = encode(&[
            Op::CursorPosition { row: 1, col: 1 },
            Op::EraseInDisplay(EraseMode::All),
        ]);
        assert_eq!(out.bytes(), expected);
    }

    #[test]
    fn set_style_matches_the_vt_encoding_in_fixed_order() {
        let mut out = VecReport::default();
        let mut screen = Screen::new(&mut out, Geometry::DEFAULT, false);
        let style = Style::fg(Color::Basic(BasicColor::Red))
            .on(Color::Basic(BasicColor::Black))
            .bold()
            .underline();
        screen.set_style(&style);
        let expected = encode(&[
            Op::Sgr(Sgr::Reset),
            Op::Sgr(Sgr::Bold),
            Op::Sgr(Sgr::Underline),
            Op::Sgr(Sgr::Foreground(Color::Basic(BasicColor::Red))),
            Op::Sgr(Sgr::Background(Color::Basic(BasicColor::Black))),
        ]);
        assert_eq!(out.bytes(), expected);
    }

    #[test]
    fn default_style_resets_and_sets_default_colours() {
        let mut out = VecReport::default();
        let mut screen = Screen::new(&mut out, Geometry::DEFAULT, false);
        screen.set_style(&Style::DEFAULT);
        let expected = encode(&[
            Op::Sgr(Sgr::Reset),
            Op::Sgr(Sgr::Foreground(Color::Default)),
            Op::Sgr(Sgr::Background(Color::Default)),
        ]);
        assert_eq!(out.bytes(), expected);
    }

    #[test]
    fn fullscreen_round_trip_matches_the_vt_encoding() {
        let mut out = VecReport::default();
        let mut screen = Screen::new(&mut out, Geometry::DEFAULT, false);
        screen.enter_fullscreen();
        screen.leave_fullscreen();
        let expected = encode(&[
            Op::EnterAltScreen,
            Op::HideCursor,
            Op::CursorPosition { row: 1, col: 1 },
            Op::EraseInDisplay(EraseMode::All),
            Op::ShowCursor,
            Op::LeaveAltScreen,
        ]);
        assert_eq!(out.bytes(), expected);
    }

    #[test]
    fn plain_mode_emits_no_escape_bytes() {
        let mut out = VecReport::default();
        let mut screen = Screen::new(&mut out, Geometry::DEFAULT, true);
        screen.enter_fullscreen();
        screen.clear();
        screen.move_to(5, 5);
        screen.set_style(&Style::fg(Color::Basic(BasicColor::Green)).bold());
        screen.reset_style();
        screen.write_str("hello");
        screen.leave_fullscreen();
        // Only the literal text survives; not a single escape byte is written.
        assert_eq!(out.bytes(), b"hello");
        assert!(!out.bytes().contains(&0x1b));
    }

    #[test]
    fn text_helpers_write_in_both_modes() {
        let mut rich = VecReport::default();
        let mut screen = Screen::new(&mut rich, Geometry::DEFAULT, false);
        screen.write_str("a");
        screen.newline();
        assert_eq!(rich.bytes(), b"a\r\n");
    }

    #[test]
    fn report_sink_flushes_across_the_chunk_boundary() {
        // Feed more bytes than one chunk holds: every byte reaches the report
        // exactly once, in order, with no overflow.
        let mut out = VecReport::default();
        let payload: alloc::vec::Vec<u8> = (0..200u32)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        {
            let mut sink = ReportSink::new(&mut out);
            sink.extend(payload.iter().copied());
            sink.flush();
        }
        assert_eq!(out.bytes(), payload.as_slice());
    }
}
