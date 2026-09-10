//! An animation's frames, composited onto one canvas and stepped in order.
//!
//! A GIF's block chain and an animated WEBP's `ANMF` chunks are both frames
//! that composite onto a retained canvas under their container's own
//! disposal model, so both are stepped, addressed, and refused the same way.
//! Only advancing the canvas by one frame differs between them, which is
//! what [`FrameSource`] is; the cursor, the remembered refusal, and the delay
//! most recently declared are shared.
//!
//! The document is handed to each call rather than held, so a walk borrows
//! nothing and a caller can own both it and the bytes it reads.

use crate::DecodeError;

/// Where an animation's frames come from.
pub(crate) trait FrameSource {
    /// The canvas width every frame composites onto.
    fn width(&self) -> u32;

    /// The canvas height every frame composites onto.
    fn height(&self) -> u32;

    /// How many frames the container declares.
    fn count(&self) -> u32;

    /// How many times the container asks for the sequence to be played;
    /// `None` for ever.
    fn loop_count(&self) -> Option<u32>;

    /// The composited canvas as it stands: straight-alpha RGBA8, exactly
    /// `width * height * 4` bytes.
    fn canvas(&self) -> &[u8];

    /// Composite the frame at `index` of `bytes` onto the canvas and answer
    /// the delay it declares, in nanoseconds.
    ///
    /// Called in order and only while frames remain, so an implementor never
    /// answers for the end of the chain. A source that walks a chain already
    /// knows where it is and ignores `index`; one that indexes a table of
    /// frames reads it rather than keeping a second cursor.
    fn advance(&mut self, bytes: &[u8], index: u32) -> Result<u64, DecodeError>;

    /// Clear the canvas and return to the first frame.
    fn restart(&mut self);
}

/// An animation's frames, composited one at a time.
pub(crate) struct Animation<S> {
    source: S,
    cursor: u32,
    delay_ns: u64,
    /// The refusal a step stopped at, if one did.
    ///
    /// A part-way refusal leaves the previous frame's disposal applied and
    /// may leave part of the refused frame's pixels on the canvas, so nothing
    /// there describes a whole frame any more. Recording the refusal is what
    /// keeps a later frame from ever being composited onto that state.
    failed: Option<DecodeError>,
}

impl<S: FrameSource> Animation<S> {
    /// Prepare to step `source`'s frames, compositing none of them.
    pub(crate) const fn new(source: S) -> Self {
        Self {
            source,
            cursor: 0,
            delay_ns: 0,
            failed: None,
        }
    }

    pub(crate) fn width(&self) -> u32 {
        self.source.width()
    }

    pub(crate) fn height(&self) -> u32 {
        self.source.height()
    }

    pub(crate) fn count(&self) -> u32 {
        self.source.count()
    }

    pub(crate) fn loop_count(&self) -> Option<u32> {
        self.source.loop_count()
    }

    /// The frame a step would composite next.
    pub(crate) const fn index(&self) -> u32 {
        self.cursor
    }

    /// The delay the frame most recently stepped to declares.
    pub(crate) const fn delay_ns(&self) -> u64 {
        self.delay_ns
    }

    pub(crate) fn canvas(&self) -> &[u8] {
        self.source.canvas()
    }

    /// Which frame the canvas holds, or `None` where it holds no whole one
    /// — before the first step, and after a refusal left part of a frame on
    /// it.
    pub(crate) fn held(&self) -> Option<u32> {
        if self.failed.is_some() {
            return None;
        }
        self.cursor.checked_sub(1)
    }

    /// Composite the next frame, answering `false` once they are exhausted.
    ///
    /// A refusal is remembered and repeated until [`Self::rewind`], which is
    /// the same contract every container's walk keeps.
    pub(crate) fn step(&mut self, bytes: &[u8]) -> Result<bool, DecodeError> {
        if let Some(failed) = &self.failed {
            return Err(failed.clone());
        }
        if self.cursor >= self.source.count() {
            return Ok(false);
        }
        match self.source.advance(bytes, self.cursor) {
            Ok(delay_ns) => {
                self.delay_ns = delay_ns;
                self.cursor += 1;
                Ok(true)
            }
            Err(err) => {
                self.failed = Some(err.clone());
                Err(err)
            }
        }
    }

    /// Composite forward to the frame at `index`, answering `false` when
    /// there is none.
    ///
    /// A frame composites onto its predecessors, so frame `index` *is* the
    /// canvas after compositing every frame up to it. The canvas already
    /// holds the frame before the cursor, so reaching a later one only
    /// composites the frames in between: playing an animation through by
    /// address costs each frame one decode rather than one per frame that
    /// precedes it. Only going back needs the canvas cleared and the walk
    /// replayed, and so does a remembered refusal, which left the canvas
    /// describing no whole frame at all.
    pub(crate) fn frame(&mut self, bytes: &[u8], index: u32) -> Result<bool, DecodeError> {
        if self.failed.is_some() || self.cursor > index.saturating_add(1) {
            self.rewind();
        }
        while self.cursor <= index {
            if !self.step(bytes)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Restart at the first frame, clearing the canvas and a remembered
    /// refusal.
    pub(crate) fn rewind(&mut self) {
        self.source.restart();
        self.cursor = 0;
        self.delay_ns = 0;
        self.failed = None;
    }
}

#[cfg(test)]
#[path = "frames_tests.rs"]
mod tests;
