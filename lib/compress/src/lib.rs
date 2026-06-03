//! First-party RustOS LZ compression codec (`lib/compress`).
//!
//! This crate is the single shared home of RustOS's general-purpose,
//! lossless compression (`AGENTS.md` §16.4 lists compression as a curated
//! shared-library class). `RustFS` uses it to compress every file-data record
//! before encryption (`docs/src/filesystem/rustfs-spec.md` §6, §10), and it
//! is written here rather than pulled from a registry because §2.12 — *roll
//! your own; do not trust external code* — bars an external
//! `zstd`/`lz4`/compression dependency. This is **not** the crypto carve-out:
//! the codec is entirely first-party.
//!
//! # The codec
//!
//! [`compress`] and [`decompress`] implement a low-CPU, byte-oriented LZ77
//! codec in the spirit of the zstd "fast" / LZ4 profiles
//! (`docs/src/filesystem/rustfs-spec.md` §10 — *the v1 target is a low-CPU
//! zstd-fast-style profile, not maximum ratio*). The encoder is a single
//! greedy pass over a small hash table of recent 4-byte sequences; the
//! decoder is a tight literal-copy / match-copy loop. There is no entropy
//! stage, so it is fast and predictable rather than maximally dense.
//!
//! The on-the-wire frame is:
//!
//! ```text
//! [ "RLZ1" magic : 4 ][ uncompressed length : u32 LE : 4 ][ sequences... ]
//! ```
//!
//! Each *sequence* is an LZ4-style token: a one-byte token splitting into a
//! literal run length (high nibble) and a match length code (low nibble),
//! optional 0xFF-continuation length extensions, the literal bytes, and —
//! unless the sequence is the final literal-only run — a little-endian
//! [`u16`] back-reference offset and the match length. Matches may overlap
//! their source (run-length expansion), copied one byte at a time.
//!
//! # Safety and robustness (`AGENTS.md` §2.9, §19.6)
//!
//! The crate is `no_std`, allocates nothing, and contains no `unsafe`. Both
//! entry points are `Result`-based and total: a malformed, truncated, or
//! adversarial compressed stream returns [`Error::Corrupt`], never a panic,
//! and the declared output length is bounds-checked against the caller's
//! destination *before* any byte is produced, so memory is bounded before
//! work begins (`docs/src/filesystem/rustfs-spec.md` §10 — *bound memory
//! before allocation; malformed compressed data returns an error, never
//! panic*).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(test)]
mod tests;

/// Frame magic identifying a RustOS LZ stream (`RLZ1`, format version 1).
const MAGIC: [u8; 4] = *b"RLZ1";

/// Bytes of fixed frame header: the [`MAGIC`] plus the [`u32`] little-endian
/// uncompressed length.
const HEADER_LEN: usize = 4 + 4;

/// Shortest back-reference match the encoder will emit. A match must beat the
/// bytes it would otherwise cost as literals plus the offset, so three or
/// fewer matched bytes never pays for its two-byte offset.
const MIN_MATCH: usize = 4;

/// Largest back-reference distance, bounded by the [`u16`] offset field.
const MAX_OFFSET: usize = 65_535;

/// Token-nibble sentinel meaning "read length-extension bytes".
const NIBBLE_MAX: usize = 15;

/// 0xFF continuation byte used to extend a length past a nibble.
const EXT_FULL: u8 = 0xFF;

/// Log2 of the match-finder hash table. A 4096-entry table is a few
/// kilobytes of stack and is ample for the kilobyte-scale records `RustFS`
/// compresses; a larger table buys little for a fast profile.
const HASH_LOG: u32 = 12;

/// Number of slots in the match-finder hash table.
const HASH_SIZE: usize = 1 << HASH_LOG;

/// Empty-slot sentinel in the match-finder hash table.
const NIL: u32 = u32::MAX;

/// Knuth multiplicative-hash constant for the 4-byte match key.
const HASH_MUL: u32 = 2_654_435_761;

/// Why a codec call failed. Both variants are recoverable: a caller stores
/// the record raw on a [`Self::TooSmall`] from [`compress`] (the
/// "compression did not win" path), and treats a [`Self::Corrupt`] from
/// [`decompress`] as data corruption
/// (`docs/src/filesystem/rustfs-spec.md` §10).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// The destination buffer was too small to hold the output. From
    /// [`compress`] this also signals "the input did not compress" — the
    /// caller falls back to storing the record raw.
    TooSmall,
    /// The compressed input was malformed, truncated, or inconsistent (bad
    /// magic, an out-of-range back-reference, a length that overruns the
    /// frame, or a declared length the stream does not produce). Decompression
    /// fails closed rather than panicking (`AGENTS.md` §2.9).
    Corrupt,
}

/// A safe upper bound on the [`compress`] output size for an `input_len`-byte
/// input, so a caller can size a scratch buffer that never provokes a
/// spurious [`Error::TooSmall`].
///
/// The worst case is wholly incompressible data stored as literals: the
/// frame header, every input byte verbatim, and the literal-length tokens
/// and 0xFF continuations that frame them.
#[must_use]
pub fn max_compressed_len(input_len: usize) -> usize {
    // One token per 255 literals, plus a continuation byte per 255 literals,
    // plus the always-present first token. Saturating throughout: an absurd
    // `input_len` yields `usize::MAX`, never a wrapped small bound.
    let framing = input_len
        .saturating_add(input_len / 255)
        .saturating_add(input_len / 255)
        .saturating_add(2);
    HEADER_LEN.saturating_add(framing)
}

/// Read a little-endian [`u32`] from `buf` at `off`. The caller guarantees
/// `off + 4 <= buf.len()`.
fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// Narrow a `usize` to `u32`, clamping at [`u32::MAX`]. Used only for
/// hash-table positions, where the clamp sentinel collides harmlessly with
/// [`NIL`] (an out-of-window position is rejected by the offset check).
fn clamp_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Widen a `u32` to `usize`. Infallible on every RustOS target (pointers are
/// at least 32-bit); the clamp keeps it total without a panic regardless.
fn to_usize(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// The 4-byte match-finder hash of the window starting at `src[pos]`. The
/// caller guarantees `pos + 4 <= src.len()`.
fn hash4(src: &[u8], pos: usize) -> usize {
    let key = read_u32_le(src, pos);
    to_usize(key.wrapping_mul(HASH_MUL) >> (32 - HASH_LOG))
}

/// Append a length that exceeds a token nibble as 0xFF continuation bytes
/// followed by the remainder. Returns the next write position, or
/// [`Error::TooSmall`] if `dst` cannot hold the continuation.
fn write_length_ext(dst: &mut [u8], mut pos: usize, mut remaining: usize) -> Result<usize, Error> {
    while remaining >= 255 {
        *dst.get_mut(pos).ok_or(Error::TooSmall)? = EXT_FULL;
        pos += 1;
        remaining -= 255;
    }
    // A `remaining` of 0..=254 is written as the final, non-0xFF byte. The
    // `u8` conversion cannot fail because `remaining < 255` here.
    let last = u8::try_from(remaining).unwrap_or(EXT_FULL);
    *dst.get_mut(pos).ok_or(Error::TooSmall)? = last;
    Ok(pos + 1)
}

/// Read a continuation-encoded length extension from `src` starting at `pos`.
/// Returns `(extra, next_pos)` or [`Error::Corrupt`] on a truncated run.
fn read_length_ext(src: &[u8], mut pos: usize) -> Result<(usize, usize), Error> {
    let mut extra: usize = 0;
    loop {
        let byte = *src.get(pos).ok_or(Error::Corrupt)?;
        pos += 1;
        extra = extra.checked_add(usize::from(byte)).ok_or(Error::Corrupt)?;
        if byte != EXT_FULL {
            return Ok((extra, pos));
        }
    }
}

/// Emit one sequence: the `literals`, then — when `mat` is `Some((offset,
/// match_len))` — the back-reference. The final sequence of a frame passes
/// `mat = None`. Returns the next write position in `dst`.
fn emit_sequence(
    dst: &mut [u8],
    mut pos: usize,
    literals: &[u8],
    mat: Option<(usize, usize)>,
) -> Result<usize, Error> {
    let lit_len = literals.len();
    let lit_nibble = lit_len.min(NIBBLE_MAX);
    let (match_excess, match_nibble) = match mat {
        Some((_, match_len)) => {
            let excess = match_len - MIN_MATCH;
            (Some(excess), excess.min(NIBBLE_MAX))
        }
        None => (None, 0),
    };

    // Token: literal nibble in the high four bits, match nibble in the low.
    let token = u8::try_from((lit_nibble << 4) | match_nibble).unwrap_or(0xFF);
    *dst.get_mut(pos).ok_or(Error::TooSmall)? = token;
    pos += 1;

    if lit_nibble == NIBBLE_MAX {
        pos = write_length_ext(dst, pos, lit_len - NIBBLE_MAX)?;
    }

    let end = pos.checked_add(lit_len).ok_or(Error::TooSmall)?;
    dst.get_mut(pos..end)
        .ok_or(Error::TooSmall)?
        .copy_from_slice(literals);
    pos = end;

    if let (Some((offset, _)), Some(excess)) = (mat, match_excess) {
        let off = u16::try_from(offset).map_err(|_| Error::TooSmall)?;
        let off_bytes = off.to_le_bytes();
        *dst.get_mut(pos).ok_or(Error::TooSmall)? = off_bytes[0];
        *dst.get_mut(pos + 1).ok_or(Error::TooSmall)? = off_bytes[1];
        pos += 2;
        if match_nibble == NIBBLE_MAX {
            pos = write_length_ext(dst, pos, excess - NIBBLE_MAX)?;
        }
    }

    Ok(pos)
}

/// Compress `src` into `dst`, returning the number of bytes written.
///
/// The codec is a single greedy LZ77 pass: a hash table maps the most recent
/// position of each 4-byte sequence, and a confirmed back-reference within the
/// 64 KiB window is extended and emitted, otherwise the byte is deferred as a
/// literal. Compression "winning" is the caller's policy — `RustFS` keeps the
/// raw record when the compressed form is not smaller
/// (`docs/src/filesystem/rustfs-spec.md` §10) — so this function does not
/// itself reject a non-shrinking result; it only fails when `dst` is too
/// small. Size `dst` with [`max_compressed_len`] to guarantee success.
///
/// # Errors
///
/// [`Error::TooSmall`] if `dst` cannot hold the compressed frame.
pub fn compress(src: &[u8], dst: &mut [u8]) -> Result<usize, Error> {
    // Header: magic + uncompressed length. A length above `u32::MAX` cannot
    // be framed; such an input is rejected rather than silently truncated.
    let len = src.len();
    let len32 = u32::try_from(len).map_err(|_| Error::TooSmall)?;
    if dst.len() < HEADER_LEN {
        return Err(Error::TooSmall);
    }
    dst[0..4].copy_from_slice(&MAGIC);
    dst[4..8].copy_from_slice(&len32.to_le_bytes());
    let mut out = HEADER_LEN;

    let mut table = [NIL; HASH_SIZE];
    let mut anchor = 0usize;
    let mut ip = 0usize;

    // The last `MIN_MATCH - 1` positions cannot start a 4-byte match, so the
    // scan stops there and the tail is emitted as the closing literal run.
    while ip + MIN_MATCH <= len {
        let slot = hash4(src, ip);
        let candidate = table[slot];
        table[slot] = clamp_u32(ip);

        if candidate != NIL {
            let cpos = to_usize(candidate);
            let offset = ip - cpos;
            if (1..=MAX_OFFSET).contains(&offset)
                && src[cpos..cpos + MIN_MATCH] == src[ip..ip + MIN_MATCH]
            {
                let mut match_len = MIN_MATCH;
                while ip + match_len < len && src[cpos + match_len] == src[ip + match_len] {
                    match_len += 1;
                }
                out = emit_sequence(dst, out, &src[anchor..ip], Some((offset, match_len)))?;
                ip += match_len;
                anchor = ip;
                continue;
            }
        }
        ip += 1;
    }

    // Closing literal-only sequence (everything from the anchor to the end).
    out = emit_sequence(dst, out, &src[anchor..len], None)?;
    Ok(out)
}

/// Decompress the LZ frame in `src` into `dst`, returning the number of bytes
/// written.
///
/// The declared uncompressed length is read from the frame header and checked
/// against `dst.len()` before any output is produced, so memory is bounded up
/// front (`docs/src/filesystem/rustfs-spec.md` §10). Every literal copy,
/// back-reference offset, and match length is validated against the frame and
/// the bytes produced so far.
///
/// # Errors
///
/// * [`Error::TooSmall`] if `dst` is smaller than the declared output length.
/// * [`Error::Corrupt`] for a bad magic, a truncated stream, an
///   out-of-range back-reference, or a frame that does not produce exactly
///   the declared number of bytes.
pub fn decompress(src: &[u8], dst: &mut [u8]) -> Result<usize, Error> {
    if src.len() < HEADER_LEN || src[0..4] != MAGIC {
        return Err(Error::Corrupt);
    }
    let target = to_usize(read_u32_le(src, 4));
    if target > dst.len() {
        return Err(Error::TooSmall);
    }

    let mut ip = HEADER_LEN;
    let mut op = 0usize;

    while op < target {
        let token = *src.get(ip).ok_or(Error::Corrupt)?;
        ip += 1;
        let mut lit_len = usize::from(token >> 4);
        let mut match_nibble = usize::from(token & 0x0F);

        if lit_len == NIBBLE_MAX {
            let (extra, next) = read_length_ext(src, ip)?;
            ip = next;
            lit_len = lit_len.checked_add(extra).ok_or(Error::Corrupt)?;
        }

        // Copy the literal run, bounds-checked against frame and destination.
        let lit_end_src = ip.checked_add(lit_len).ok_or(Error::Corrupt)?;
        let lit_end_dst = op.checked_add(lit_len).ok_or(Error::Corrupt)?;
        if lit_end_src > src.len() || lit_end_dst > target {
            return Err(Error::Corrupt);
        }
        dst[op..lit_end_dst].copy_from_slice(&src[ip..lit_end_src]);
        ip = lit_end_src;
        op = lit_end_dst;

        if op == target {
            // Final sequence: literals only, no trailing back-reference.
            break;
        }

        // A back-reference must follow: a 2-byte offset then the match length.
        let lo = *src.get(ip).ok_or(Error::Corrupt)?;
        let hi = *src.get(ip + 1).ok_or(Error::Corrupt)?;
        ip += 2;
        let offset = usize::from(u16::from_le_bytes([lo, hi]));
        if match_nibble == NIBBLE_MAX {
            let (extra, next) = read_length_ext(src, ip)?;
            ip = next;
            match_nibble = match_nibble.checked_add(extra).ok_or(Error::Corrupt)?;
        }
        let match_len = match_nibble.checked_add(MIN_MATCH).ok_or(Error::Corrupt)?;

        // The back-reference must point inside the bytes already produced.
        if offset == 0 || offset > op {
            return Err(Error::Corrupt);
        }
        let match_end = op.checked_add(match_len).ok_or(Error::Corrupt)?;
        if match_end > target {
            return Err(Error::Corrupt);
        }
        // Copy byte-by-byte so overlapping (run-length) matches expand.
        let mut from = op - offset;
        while op < match_end {
            dst[op] = dst[from];
            op += 1;
            from += 1;
        }
    }

    if op != target {
        return Err(Error::Corrupt);
    }
    Ok(op)
}
