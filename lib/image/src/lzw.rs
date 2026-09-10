//! The LZW dictionary and expansion loop.
//!
//! GIF and TIFF both code their pixels with LZW over a dictionary of at most
//! 4096 entries whose first `1 << root_bits` codes are the literals, whose
//! next code clears the table and whose next again ends the stream. What
//! differs is only how codes are packed into bytes and when a new entry
//! widens the code that follows it, so those are the caller's
//! ([`CodeSource`], [`Widen`]) and everything else — the dictionary, the
//! string walk, the entry a code defines for itself, and the deferred clear
//! real encoders rely on — is defined once here.

use alloc::vec::Vec;

use tairix_util::fallible;

use crate::DecodeError;

/// The widest code either dialect reaches, and the resulting table size.
pub(crate) const MAX_CODE_BITS: u32 = 12;
pub(crate) const MAX_CODES: usize = 1 << MAX_CODE_BITS;

/// The prefix stored for a root code, which has none. Outside the code
/// space, so it can never be mistaken for a code.
const NO_PREFIX: u16 = u16::MAX;

/// Where a stream's codes come from.
///
/// The packing is the caller's because it is what the two dialects disagree
/// on: GIF runs a least-significant-bit-first stream across a chain of data
/// sub-blocks, TIFF a flat most-significant-bit-first run of bytes.
pub(crate) trait CodeSource {
    /// The next `width`-bit code, or `None` once the stream has run out.
    fn code(&mut self, width: u32) -> Result<Option<u16>, DecodeError>;
}

/// When the entry a step defines widens the code that follows it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Widen {
    /// Once the next free code no longer fits the current width. GIF's
    /// schedule, and the one older TIFF writers emit.
    WhenFull,
    /// One code earlier, so the largest code ever read at a given width is
    /// one below the width's own ceiling. TIFF's own schedule.
    OneEarly,
}

impl Widen {
    /// Whether a table whose next free code is `next` needs a wider read.
    const fn reached(self, next: u16, width: u32) -> bool {
        let next = next as u32;
        match self {
            Self::WhenFull => next >= 1 << width,
            Self::OneEarly => next + 1 >= 1 << width,
        }
    }
}

/// The dictionary and output stack, allocated once and reused by every
/// stream a decode expands.
pub(crate) struct Lzw {
    prefix: Vec<u16>,
    suffix: Vec<u8>,
    /// One slot deeper than the longest possible string, for the reserved
    /// leading byte the not-yet-defined-code case fills in.
    stack: Vec<u8>,
}

impl Lzw {
    pub(crate) fn new() -> Option<Self> {
        Some(Self {
            prefix: fallible::filled(MAX_CODES, NO_PREFIX)?,
            suffix: fallible::filled(MAX_CODES, 0u8)?,
            stack: fallible::filled(MAX_CODES + 1, 0u8)?,
        })
    }

    /// Walk `code`'s string onto the stack in reverse from `depth`, answering
    /// its first byte.
    ///
    /// A dictionary entry's prefix is always a code that already existed when
    /// the entry was defined, so the walk strictly decreases and terminates.
    /// The index and depth are checked anyway, so a table this decoder could
    /// not have built still cannot overrun either buffer.
    fn walk(
        &mut self,
        code: u16,
        roots: u16,
        depth: &mut usize,
        invalid: &DecodeError,
    ) -> Result<u8, DecodeError> {
        let mut cursor = code;
        while cursor >= roots {
            let index = usize::from(cursor);
            if index >= self.suffix.len() || *depth >= self.stack.len() {
                return Err(invalid.clone());
            }
            self.stack[*depth] = self.suffix[index];
            *depth += 1;
            cursor = self.prefix[index];
        }
        if *depth >= self.stack.len() {
            return Err(invalid.clone());
        }
        let root = u8::try_from(cursor).map_err(|_| invalid.clone())?;
        self.stack[*depth] = root;
        *depth += 1;
        Ok(root)
    }

    /// Expand `source`'s codes into `out`, answering how many bytes were
    /// written.
    ///
    /// The table grows in lockstep with the encoder under `widen`'s
    /// schedule. A stream that fills the table and never clears it keeps
    /// decoding at the widest code against the table as it stands — the
    /// deferred clear real encoders rely on — rather than being refused. One
    /// producing more bytes than `out` holds stops at its end, because the
    /// rest is not part of what was asked for; a caller that requires `out`
    /// filled compares the count it gets back.
    pub(crate) fn expand(
        &mut self,
        source: &mut impl CodeSource,
        root_bits: u32,
        widen: Widen,
        invalid: &DecodeError,
        out: &mut [u8],
    ) -> Result<usize, DecodeError> {
        let roots = 1u16 << root_bits;
        let end = roots + 1;
        for index in 0..usize::from(roots) {
            self.prefix[index] = NO_PREFIX;
            self.suffix[index] = u8::try_from(index).map_err(|_| invalid.clone())?;
        }
        let mut next = end + 1;
        let mut width = root_bits + 1;
        let mut previous: Option<u16> = None;
        let mut written = 0usize;
        while let Some(code) = source.code(width)? {
            if code == roots {
                next = end + 1;
                width = root_bits + 1;
                previous = None;
                continue;
            }
            if code == end {
                break;
            }
            let mut depth = 0usize;
            let first = match code.cmp(&next) {
                core::cmp::Ordering::Less => self.walk(code, roots, &mut depth, invalid)?,
                // The code this very step defines: its string is the previous
                // one followed by that string's own first byte. The stack
                // fills in reverse, so the trailing byte takes the slot below
                // the walk — which is why the stack carries one extra.
                core::cmp::Ordering::Equal => {
                    let Some(prev) = previous else {
                        return Err(invalid.clone());
                    };
                    depth = 1;
                    let first = self.walk(prev, roots, &mut depth, invalid)?;
                    self.stack[0] = first;
                    first
                }
                core::cmp::Ordering::Greater => return Err(invalid.clone()),
            };
            for slot in (0..depth).rev() {
                if written == out.len() {
                    break;
                }
                out[written] = self.stack[slot];
                written += 1;
            }
            if written == out.len() {
                break;
            }
            if let Some(prev) = previous {
                if usize::from(next) < MAX_CODES {
                    self.prefix[usize::from(next)] = prev;
                    self.suffix[usize::from(next)] = first;
                    next += 1;
                    if widen.reached(next, width) && width < MAX_CODE_BITS {
                        width += 1;
                    }
                }
            }
            previous = Some(code);
        }
        Ok(written)
    }
}
