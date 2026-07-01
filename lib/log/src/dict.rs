//! Segment-local string compression for log records.
//!
//! Provenance and message strings in a log stream repeat heavily: a segment
//! holding thousands of records from `kernel.mem` stores that source name once
//! and references it thereafter. This module is the codec that achieves that
//! without a registry, a template file, or any global identifier — the
//! compression is *private to one segment* and derived entirely from the
//! record bytes already in it.
//!
//! # Back-reference model (no separate on-disk dictionary block)
//!
//! A dictionary-eligible string is encoded as one of three forms, each led by
//! a single marker byte:
//!
//! * [`FORM_PLAIN`] — the bytes inline; not remembered. High-cardinality or
//!   one-off strings (request ids, paths, digests) take this form so they never
//!   pollute the handle space.
//! * [`FORM_DEF`] — the bytes inline **and** assigned the next segment-local
//!   handle. Both encoder and decoder walk a segment's strings in the
//!   same field-and-record order, so the two assign identical handles without
//!   storing a handle number on the wire.
//! * [`FORM_REF`] — a `u16` handle referring back to a string an earlier
//!   [`FORM_DEF`] defined in the *same* segment.
//!
//! Because a handle only ever refers to a string carried inline earlier in the
//! segment, the dictionary needs no block of its own and no separate digest:
//! it is reconstructed by reading the records in order, and it is already
//! covered by the record hash chain and the segment hash that protect those
//! bytes. Tampering with a defining string breaks the chain exactly as
//! tampering with any other record byte does.
//!
//! # Bounded growth and promote-on-repeat
//!
//! The writer must not let a flood of unique short strings exhaust the
//! dictionary. Two bounds enforce that:
//!
//! * a string is promoted to a handle only on its **second** sighting in the
//!   segment (a genuinely unique string is emitted [`FORM_PLAIN`] once and
//!   never remembered), and
//! * both the promoted-entry table and the seen-once candidate table are
//!   fixed-capacity ([`MAX_ENTRIES`] / [`MAX_CANDIDATES`]) over a fixed byte
//!   arena ([`ARENA_BYTES`]); once full, further strings simply stay inline.
//!
//! Strings longer than [`MAX_DICT_STRING`] are never remembered regardless, so
//! the arena bounds the largest thing worth interning.
//!
//! The decoder mirrors the bounds and is **fail-closed**: a [`FORM_DEF`] past
//! [`MAX_ENTRIES`] or a [`FORM_REF`] to an undefined handle is rejected, never
//! guessed at.

use rustos_abi::Errno;

use crate::cursor::{put_bytes, put_u16, put_u8, read_u16, read_u8, take};

/// Inline bytes, not interned.
pub const FORM_PLAIN: u8 = 0x00;
/// Inline bytes that define the next handle.
pub const FORM_DEF: u8 = 0x01;
/// A `u16` back-reference to an earlier definition.
pub const FORM_REF: u8 = 0x02;

/// Longest string that may be interned. Longer strings are always inline.
pub const MAX_DICT_STRING: usize = 128;

/// Maximum number of promoted dictionary entries per segment.
pub const MAX_ENTRIES: usize = 256;

/// Maximum number of seen-once candidates tracked at any time.
pub const MAX_CANDIDATES: usize = 256;

/// Fixed byte arena backing the builder's remembered strings.
pub const ARENA_BYTES: usize = 16 * 1024;

/// A remembered string: an `(offset, len)` window into the builder arena.
#[derive(Copy, Clone)]
struct Span {
    offset: u32,
    len: u16,
}

/// Writer-side segment dictionary.
///
/// One builder backs one segment. It decides, for each string, whether to
/// carry it inline, promote it to a handle, or reference an existing handle,
/// and it enforces the bounded-growth and promote-on-repeat policy. It holds
/// no reference to any string it is given: an interned string is copied into
/// the arena so later records can be compared against it, which is why the
/// builder owns a fixed byte arena rather than borrowing.
///
/// The tables are scanned linearly; both are capped at a few hundred short
/// entries, and the builder is owned by the userland journal service, not the
/// kernel hot path.
pub struct DictionaryBuilder {
    arena: [u8; ARENA_BYTES],
    arena_len: usize,
    entries: [Span; MAX_ENTRIES],
    entry_count: usize,
    candidates: [Span; MAX_CANDIDATES],
    candidate_count: usize,
}

impl Default for DictionaryBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl DictionaryBuilder {
    /// A fresh, empty dictionary for a new segment.
    #[must_use]
    pub const fn new() -> Self {
        let empty = Span { offset: 0, len: 0 };
        Self {
            arena: [0u8; ARENA_BYTES],
            arena_len: 0,
            entries: [empty; MAX_ENTRIES],
            entry_count: 0,
            candidates: [empty; MAX_CANDIDATES],
            candidate_count: 0,
        }
    }

    /// Number of promoted dictionary entries so far.
    #[must_use]
    pub const fn entry_count(&self) -> usize {
        self.entry_count
    }

    fn span_bytes(&self, span: Span) -> &[u8] {
        let start = span.offset as usize;
        &self.arena[start..start + span.len as usize]
    }

    fn find_entry(&self, needle: &[u8]) -> Option<usize> {
        (0..self.entry_count).find(|&i| {
            let span = self.entries[i];
            span.len as usize == needle.len() && self.span_bytes(span) == needle
        })
    }

    fn find_candidate(&self, needle: &[u8]) -> Option<usize> {
        (0..self.candidate_count).find(|&i| {
            let span = self.candidates[i];
            span.len as usize == needle.len() && self.span_bytes(span) == needle
        })
    }

    /// Copy `bytes` into the arena, returning its span, or `None` if the arena
    /// (or the `u32` offset) cannot hold it.
    fn store(&mut self, bytes: &[u8]) -> Option<Span> {
        let end = self.arena_len.checked_add(bytes.len())?;
        if end > ARENA_BYTES {
            return None;
        }
        let offset = u32::try_from(self.arena_len).ok()?;
        let len = u16::try_from(bytes.len()).ok()?;
        self.arena[self.arena_len..end].copy_from_slice(bytes);
        self.arena_len = end;
        Some(Span { offset, len })
    }

    /// Encode `s` into `out` at `*pos` in dictionary-coded form, bounded by
    /// `max`, advancing `*pos`.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `s` exceeds `max`; [`Errno::BufferTooSmall`]
    /// if `out` cannot hold the encoded form.
    pub fn encode_str(
        &mut self,
        out: &mut [u8],
        pos: &mut usize,
        s: &str,
        max: usize,
    ) -> Result<(), Errno> {
        if s.len() > max {
            return Err(Errno::LengthOutOfRange);
        }
        let bytes = s.as_bytes();

        // Not eligible to intern: too long, or empty (no point). Inline, forget.
        if bytes.is_empty() || bytes.len() > MAX_DICT_STRING {
            return emit_inline(out, pos, FORM_PLAIN, bytes);
        }

        if let Some(handle) = self.find_entry(bytes) {
            put_u8(out, pos, FORM_REF)?;
            // `handle < entry_count <= MAX_ENTRIES`, so this conversion always
            // succeeds; the checked form keeps that guarantee explicit.
            let handle = u16::try_from(handle).map_err(|_| Errno::LengthOutOfRange)?;
            return put_u16(out, pos, handle);
        }

        if let Some(cand_idx) = self.find_candidate(bytes) {
            // Second sighting: promote to a handle if there is room.
            if self.entry_count < MAX_ENTRIES {
                let span = self.candidates[cand_idx];
                self.entries[self.entry_count] = span;
                self.entry_count += 1;
                // Swap-remove the promoted candidate.
                self.candidate_count -= 1;
                self.candidates[cand_idx] = self.candidates[self.candidate_count];
                return emit_inline(out, pos, FORM_DEF, bytes);
            }
            // Dictionary full: stay inline, leave it a candidate.
            return emit_inline(out, pos, FORM_PLAIN, bytes);
        }

        // First sighting: remember it as a candidate if there is room.
        if self.candidate_count < MAX_CANDIDATES {
            if let Some(span) = self.store(bytes) {
                self.candidates[self.candidate_count] = span;
                self.candidate_count += 1;
            }
        }
        emit_inline(out, pos, FORM_PLAIN, bytes)
    }
}

/// Reader-side segment dictionary.
///
/// One view backs the decode of one segment's records, which are decoded in
/// append order. It accumulates the strings [`FORM_DEF`] defines (borrowed from
/// the segment bytes) and resolves [`FORM_REF`] handles against them. It is
/// fail-closed: a definition beyond [`MAX_ENTRIES`] or a reference to an
/// undefined handle is rejected.
pub struct DictionaryView<'a> {
    entries: [&'a str; MAX_ENTRIES],
    count: usize,
}

impl Default for DictionaryView<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> DictionaryView<'a> {
    /// A fresh, empty view for a new segment.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: [""; MAX_ENTRIES],
            count: 0,
        }
    }

    /// Number of definitions accumulated so far.
    #[must_use]
    pub const fn entry_count(&self) -> usize {
        self.count
    }

    /// Decode one dictionary-coded string at `*pos` in `bytes`, bounded by
    /// `max`, advancing `*pos`.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — an unknown form byte or non-UTF-8 inline bytes.
    /// * [`Errno::LengthOutOfRange`] — a truncated field, a length past `max`,
    ///   a definition past [`MAX_ENTRIES`], or a reference to an undefined
    ///   handle.
    pub fn decode_str(
        &mut self,
        bytes: &'a [u8],
        pos: &mut usize,
        max: usize,
    ) -> Result<&'a str, Errno> {
        let form = read_u8(bytes, pos)?;
        match form {
            FORM_PLAIN => read_inline(bytes, pos, max),
            FORM_DEF => {
                let s = read_inline(bytes, pos, max.min(MAX_DICT_STRING))?;
                if self.count >= MAX_ENTRIES {
                    return Err(Errno::LengthOutOfRange);
                }
                self.entries[self.count] = s;
                self.count += 1;
                Ok(s)
            }
            FORM_REF => {
                let handle = read_u16(bytes, pos)? as usize;
                if handle >= self.count {
                    return Err(Errno::LengthOutOfRange);
                }
                Ok(self.entries[handle])
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

fn emit_inline(out: &mut [u8], pos: &mut usize, form: u8, bytes: &[u8]) -> Result<(), Errno> {
    put_u8(out, pos, form)?;
    // Callers gate length against `max <= u16::MAX`; the interned path is
    // additionally capped at `MAX_DICT_STRING`.
    put_u16(
        out,
        pos,
        u16::try_from(bytes.len()).map_err(|_| Errno::LengthOutOfRange)?,
    )?;
    put_bytes(out, pos, bytes)
}

fn read_inline<'a>(bytes: &'a [u8], pos: &mut usize, max: usize) -> Result<&'a str, Errno> {
    let len = read_u16(bytes, pos)? as usize;
    if len > max {
        return Err(Errno::LengthOutOfRange);
    }
    let slice = take(bytes, pos, len)?;
    core::str::from_utf8(slice).map_err(|_| Errno::OutOfRange)
}

#[cfg(test)]
mod tests {
    use super::{
        DictionaryBuilder, DictionaryView, FORM_DEF, FORM_PLAIN, FORM_REF, MAX_DICT_STRING,
        MAX_ENTRIES,
    };
    use rustos_abi::Errno;

    // A wide bound so the codec, not the field limit, drives every test.
    const MAX: usize = 256;

    /// Encode a whole sequence of strings through one builder, then decode the
    /// same sequence through one view, asserting every string round-trips and
    /// the reader consumes exactly the bytes the writer produced.
    fn round_trip(strings: &[&str]) -> (usize, usize) {
        let mut buf = [0u8; 8192];
        let mut builder = DictionaryBuilder::new();
        let mut wpos = 0usize;
        for s in strings {
            builder
                .encode_str(&mut buf, &mut wpos, s, MAX)
                .expect("encodes");
        }

        let mut view = DictionaryView::new();
        let mut rpos = 0usize;
        for s in strings {
            let got = view
                .decode_str(&buf[..wpos], &mut rpos, MAX)
                .expect("decodes");
            assert_eq!(got, *s);
        }
        assert_eq!(rpos, wpos, "reader consumes exactly what the writer wrote");
        assert_eq!(builder.entry_count(), view.entry_count());
        (wpos, builder.entry_count())
    }

    #[test]
    fn distinct_strings_round_trip_uninterned() {
        let (_, entries) = round_trip(&["kernel.mem", "net0", "/Storage/a", "service.dhcp"]);
        // Nothing repeated, so nothing is promoted.
        assert_eq!(entries, 0);
    }

    #[test]
    fn repeats_round_trip_and_are_promoted() {
        // First "kernel.mem" is a candidate (plain), second promotes it (def),
        // the rest reference the handle.
        let seq = ["kernel.mem"; 6];
        let (size, entries) = round_trip(&seq);
        assert_eq!(entries, 1, "one promoted entry");
        // First byte is a plain form, the second a definition, the third a ref.
        let mut buf = [0u8; 8192];
        let mut b = DictionaryBuilder::new();
        let mut p = 0;
        let mut offs = [0usize; 3];
        for (i, off) in offs.iter_mut().enumerate() {
            *off = p;
            b.encode_str(&mut buf, &mut p, seq[i], MAX).unwrap();
        }
        assert_eq!(buf[offs[0]], FORM_PLAIN);
        assert_eq!(buf[offs[1]], FORM_DEF);
        assert_eq!(buf[offs[2]], FORM_REF);
        // A reference is 3 bytes; the naive inline form is 3 + len.
        let naive = seq.len() * (3 + "kernel.mem".len());
        assert!(
            size < naive,
            "dictionary compresses repeats: {size} < {naive}"
        );
    }

    #[test]
    fn repeat_within_one_encode_pass_is_a_ref() {
        // A string that recurs across fields of the same record still promotes.
        let (_, entries) = round_trip(&["dhcp", "dhcp", "dhcp"]);
        assert_eq!(entries, 1);
    }

    #[test]
    fn long_strings_are_never_interned() {
        let long = "x".repeat(MAX_DICT_STRING + 1);
        let (_, entries) = round_trip(&[long.as_str(), long.as_str(), long.as_str()]);
        assert_eq!(entries, 0, "a string past the intern cap stays inline");
    }

    #[test]
    fn empty_strings_are_never_interned() {
        let (_, entries) = round_trip(&["", "", ""]);
        assert_eq!(entries, 0);
    }

    #[test]
    fn a_flood_of_unique_strings_promotes_nothing() {
        let mut buf = alloc::vec![0u8; 65536];
        let mut b = DictionaryBuilder::new();
        let mut p = 0usize;
        for i in 0..2000u32 {
            let s = alloc::format!("uniq-{i}");
            b.encode_str(&mut buf, &mut p, &s, MAX).unwrap();
        }
        assert_eq!(
            b.entry_count(),
            0,
            "unique strings never repeat, never promote"
        );
    }

    #[test]
    fn promotion_is_bounded_at_max_entries() {
        let mut buf = alloc::vec![0u8; 1 << 20];
        let mut b = DictionaryBuilder::new();
        let mut p = 0usize;
        // Present each of many distinct short strings twice, so each is a
        // promotion candidate; promotion must stop at MAX_ENTRIES.
        for i in 0..(MAX_ENTRIES + 50) {
            let s = alloc::format!("s{i}");
            b.encode_str(&mut buf, &mut p, &s, MAX).unwrap();
            b.encode_str(&mut buf, &mut p, &s, MAX).unwrap();
        }
        assert_eq!(b.entry_count(), MAX_ENTRIES, "promotion is capped");
    }

    #[test]
    fn decode_rejects_an_undefined_reference() {
        // FORM_REF to handle 0 with an empty view.
        let bytes = [FORM_REF, 0x00, 0x00];
        let mut view = DictionaryView::new();
        let mut pos = 0usize;
        assert_eq!(
            view.decode_str(&bytes, &mut pos, MAX),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn decode_rejects_an_unknown_form() {
        let bytes = [0x7Fu8, 0x00, 0x00];
        let mut view = DictionaryView::new();
        let mut pos = 0usize;
        assert_eq!(
            view.decode_str(&bytes, &mut pos, MAX),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn decode_rejects_a_truncated_field() {
        // A definition claiming 4 bytes but carrying only 1.
        let bytes = [FORM_DEF, 0x04, 0x00, b'a'];
        let mut view = DictionaryView::new();
        let mut pos = 0usize;
        assert_eq!(
            view.decode_str(&bytes, &mut pos, MAX),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn decode_rejects_a_length_past_max() {
        // A plain string of length 2 decoded with max = 1.
        let bytes = [FORM_PLAIN, 0x02, 0x00, b'a', b'b'];
        let mut view = DictionaryView::new();
        let mut pos = 0usize;
        assert_eq!(
            view.decode_str(&bytes, &mut pos, 1),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn decode_rejects_too_many_definitions() {
        // Hand-craft MAX_ENTRIES + 1 one-byte definitions; the last is refused.
        let mut buf = alloc::vec::Vec::new();
        for _ in 0..=MAX_ENTRIES {
            buf.push(FORM_DEF);
            buf.extend_from_slice(&1u16.to_le_bytes());
            buf.push(b'a');
        }
        let mut view = DictionaryView::new();
        let mut pos = 0usize;
        let mut last = Ok("");
        for _ in 0..=MAX_ENTRIES {
            last = view.decode_str(&buf, &mut pos, MAX);
        }
        assert_eq!(last, Err(Errno::LengthOutOfRange));
        assert_eq!(view.entry_count(), MAX_ENTRIES);
    }

    #[test]
    fn non_utf8_inline_bytes_are_rejected() {
        let bytes = [FORM_PLAIN, 0x01, 0x00, 0xFF];
        let mut view = DictionaryView::new();
        let mut pos = 0usize;
        assert_eq!(
            view.decode_str(&bytes, &mut pos, MAX),
            Err(Errno::OutOfRange)
        );
    }

    extern crate alloc;
}
