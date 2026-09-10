//! A container of independent pages, walked one at a time.
//!
//! An icon file's entries and a RISC OS sprite area's sprites are both
//! pictures in their own right rather than frames of one animation, so both
//! are walked and addressed the same way. Only locating and decoding a page
//! differs between them, which is what [`PageSource`] is; the cursor, the
//! remembered refusal, and the one retained decode are shared.

use crate::{DecodeError, DecodeLimits, RasterImage};

/// Where a page container's pages come from.
pub(crate) trait PageSource {
    /// How many pages the container declares.
    fn count(&self) -> u32;

    /// Decode the page at `index`, which the caller has already bounded
    /// against [`Self::count`].
    fn decode(&mut self, index: u32, limits: &DecodeLimits) -> Result<RasterImage, DecodeError>;
}

/// A page container's pages, decoded one at a time.
///
/// Nothing is weighed against `limits` when the container opens, because
/// nothing is allocated then: the pages are independent pictures and a caller
/// may well want a small one out of a file whose largest it could never
/// afford, so each page is weighed when it is asked for.
pub(crate) struct Pages<S> {
    source: S,
    limits: DecodeLimits,
    /// The largest page's size, which is the picture the container is.
    width: u32,
    height: u32,
    cursor: u32,
    /// The page most recently decoded, which is what a frame lends.
    current: Option<RasterImage>,
    /// The refusal a step stopped at, if one did.
    failed: Option<DecodeError>,
}

impl<S: PageSource> Pages<S> {
    /// Prepare to decode `source`'s pages, decoding none of them.
    pub(crate) fn new(source: S, limits: &DecodeLimits, width: u32, height: u32) -> Self {
        Self {
            source,
            limits: *limits,
            width,
            height,
            cursor: 0,
            current: None,
            failed: None,
        }
    }

    pub(crate) const fn width(&self) -> u32 {
        self.width
    }

    pub(crate) const fn height(&self) -> u32 {
        self.height
    }

    pub(crate) fn count(&self) -> u32 {
        self.source.count()
    }

    /// The page a step would decode next.
    pub(crate) const fn index(&self) -> u32 {
        self.cursor
    }

    /// The page most recently decoded.
    pub(crate) const fn current(&self) -> Option<&RasterImage> {
        self.current.as_ref()
    }

    /// Decode the next page, answering `false` once they are exhausted.
    ///
    /// A refusal is remembered and repeated until [`Self::rewind`], which is
    /// the same contract every container's walk keeps.
    pub(crate) fn step(&mut self) -> Result<bool, DecodeError> {
        if let Some(failed) = &self.failed {
            return Err(failed.clone());
        }
        if self.cursor >= self.source.count() {
            return Ok(false);
        }
        match self.decode_at(self.cursor) {
            Ok(()) => {
                self.cursor += 1;
                Ok(true)
            }
            Err(err) => {
                self.failed = Some(err.clone());
                Err(err)
            }
        }
    }

    /// Decode the page at `index`, answering `false` when there is none.
    pub(crate) fn page(&mut self, index: u32) -> Result<bool, DecodeError> {
        if index >= self.source.count() {
            return Ok(false);
        }
        self.decode_at(index)?;
        Ok(true)
    }

    /// Restart at the first page, clearing a remembered refusal.
    pub(crate) fn rewind(&mut self) {
        self.cursor = 0;
        self.current = None;
        self.failed = None;
    }

    fn decode_at(&mut self, index: u32) -> Result<(), DecodeError> {
        // The previous page's buffer is released before the next is
        // reserved, so holding one page never costs two.
        self.current = None;
        self.current = Some(self.source.decode(index, &self.limits)?);
        Ok(())
    }
}
